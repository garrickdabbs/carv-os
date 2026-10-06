//! System calls (P2.5), synchronous IPC (P2.7), notifications and IRQ delivery (P2.8), the
//! `invoke` object methods (P2.4), and the kernel-side helpers that build user processes (P2.9).
//!
//! Register convention (ADR-0003): the syscall number is in `rax`, arguments in `rdi`, `rsi`,
//! `rdx`, `r10`, `r8`, `r9`; the kernel returns a status in `rax` (0 or a [`carv_abi::Error`]
//! code) and two result words in `rdx` and `rsi`. `syscall` itself clobbers `rcx` and `r11`;
//! every other register is preserved.
//!
//! Every capability a syscall names is a slot in the calling thread's CSpace; the kernel checks
//! the capability's rights and its object's type before touching the object, and there is no
//! other way to name an object. Messages are copied in from and out to user memory through the
//! address space's own page tables (only user-accessible pages, only writable ones for output),
//! so a bad pointer is an `InvalidArgument` error, never a kernel fault.

use alloc::vec::Vec;

use carv_abi::{Error, Message, ObjectType, Syscall, init, map, method};
use carv_caps::{CSpaceError, Capability, Rights, SpaceId};

use crate::arch::x86_64::ioapic;
use crate::arch::x86_64::syscall::{SyscallFrame, UserContext};
use crate::ipc::{self, IrqLine};
use crate::kprintln;
use crate::mm::address_space::{Access, AddressSpace, MapUserError};
use crate::objects::{self, Kernel, Object, ObjectError, ObjectId, object_bytes};
use crate::scheduler::{self, Exit, Ipc, State, TICK_NS, ThreadId, Wait};

const MESSAGE_BYTES: usize = size_of::<Message>();

/// How a syscall finished.
enum Outcome {
    /// Completed at once with two result words.
    Done(u64, u64),
    /// The caller blocked; its results arrive in its control block when it is woken.
    Blocked,
    /// The caller gives up the CPU.
    Yield,
    /// The caller destroyed its own thread.
    Exited,
}

type Step = Result<Outcome, Error>;

fn status(result: Result<(u64, u64), Error>) -> [u64; 3] {
    match result {
        Ok((a, b)) => [0, a, b],
        Err(e) => [e.raw(), 0, 0],
    }
}

/// Syscall dispatcher, called by the entry stub on the thread's kernel stack with interrupts
/// disabled.
pub extern "C" fn dispatch(frame: &mut SyscallFrame) {
    if frame.rip >= carv_abi::USER_TOP {
        // `sysret` to a non-canonical address would fault in ring 0.
        user_fault(format_args!("bad syscall return address"), frame.rip);
    }
    let result = match handle(frame) {
        Ok(Outcome::Done(a, b)) => [0, a, b],
        Ok(Outcome::Blocked) => {
            scheduler::reschedule();
            objects::with(|k| k.sched.current_tcb().result)
        }
        Ok(Outcome::Yield) => {
            scheduler::reschedule();
            [0; 3]
        }
        Ok(Outcome::Exited) => {
            scheduler::reschedule();
            unreachable!("a destroyed thread was scheduled again");
        }
        Err(e) => [e.raw(), 0, 0],
    };
    frame.rax = result[0];
    frame.rdx = result[1];
    frame.rsi = result[2];
}

fn handle(frame: &SyscallFrame) -> Step {
    let call = Syscall::from_raw(frame.rax).ok_or(Error::InvalidOperation)?;
    match call {
        Syscall::DebugPutc => {
            crate::serial::write_byte(frame.rdi as u8);
            Ok(Outcome::Done(0, 0))
        }
        Syscall::Yield => Ok(Outcome::Yield),
        Syscall::Send | Syscall::Call => {
            objects::with(|k| send(k, frame.rdi, frame.rsi, call == Syscall::Call))
        }
        Syscall::Recv => objects::with(|k| {
            let ep = endpoint_cap(k, frame.rdi, Rights::READ)?;
            check_buffer(k, frame.rsi)?;
            Ok(recv(k, ep, frame.rsi))
        }),
        Syscall::ReplyRecv => objects::with(|k| reply_recv(k, frame.rdi, frame.rsi)),
        Syscall::Signal => objects::with(|k| {
            let n = typed_cap(k, frame.rdi, Rights::WRITE, ObjectType::Notification)?.object();
            signal(k, n);
            Ok(Outcome::Done(0, 0))
        }),
        Syscall::Wait => objects::with(|k| {
            let n = typed_cap(k, frame.rdi, Rights::READ, ObjectType::Notification)?.object();
            let cur = k.sched.current();
            let Some(Object::Notification(note)) = k.objects.get_mut(n) else {
                return Err(Error::InvalidCapability);
            };
            if let Some(count) = note.poll() {
                return Ok(Outcome::Done(count, 0));
            }
            note.waiters.push_back(cur);
            k.sched.block_current(Wait::Notification(n));
            Ok(Outcome::Blocked)
        }),
        Syscall::Invoke => objects::with(|k| {
            invoke(
                k,
                frame.rdi,
                frame.rsi,
                [frame.rdx, frame.r10, frame.r8, frame.r9],
            )
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// Capability lookup

fn current_space(k: &mut Kernel) -> Result<SpaceId, Error> {
    k.sched.current_tcb().cspace.ok_or(Error::InvalidCapability)
}

/// The capability in the caller's `slot`, which must carry `need`.
fn cap(k: &mut Kernel, slot: u64, need: Rights) -> Result<Capability, Error> {
    let space = current_space(k)?;
    let slot = usize::try_from(slot).map_err(|_| Error::InvalidCapability)?;
    let cap = k
        .caps
        .get(space, slot)
        .map_err(|_| Error::InvalidCapability)?;
    if !cap.rights().contains(need) {
        return Err(Error::PermissionDenied);
    }
    Ok(cap)
}

/// Like [`cap`], and the object must have type `ty`.
fn typed_cap(k: &mut Kernel, slot: u64, need: Rights, ty: ObjectType) -> Result<Capability, Error> {
    let c = cap(k, slot, need)?;
    match k.objects.get(c.object()) {
        Some(o) if o.object_type() == ty => Ok(c),
        _ => Err(Error::InvalidCapability),
    }
}

fn endpoint_cap(k: &mut Kernel, slot: u64, need: Rights) -> Result<ObjectId, Error> {
    Ok(typed_cap(k, slot, need, ObjectType::Endpoint)?.object())
}

fn endpoint(k: &mut Kernel, id: ObjectId) -> &mut ipc::Endpoint {
    match k.objects.get_mut(id) {
        Some(Object::Endpoint(e)) => e,
        _ => unreachable!("endpoint {id} checked by the caller"),
    }
}

/// An empty, in-range slot of `space`.
fn empty_slot(k: &Kernel, space: SpaceId, slot: u64) -> Result<usize, Error> {
    let slot = usize::try_from(slot).map_err(|_| Error::InvalidArgument)?;
    match k.caps.get(space, slot) {
        Err(CSpaceError::EmptySlot) => Ok(slot),
        _ => Err(Error::InvalidArgument),
    }
}

// ---------------------------------------------------------------------------------------------
// User memory

fn address_space(k: &mut Kernel, thread: ThreadId) -> Option<&mut AddressSpace> {
    let space = k.sched.thread(thread)?.space?;
    match k.objects.get_mut(space) {
        Some(Object::AddressSpace(a)) => Some(a),
        _ => None,
    }
}

fn check_buffer(k: &mut Kernel, va: u64) -> Result<(), Error> {
    let cur = k.sched.current();
    match address_space(k, cur) {
        Some(a) if a.accessible(va, MESSAGE_BYTES, Access::UserWrite) => Ok(()),
        _ => Err(Error::InvalidArgument),
    }
}

fn read_message(k: &mut Kernel, thread: ThreadId, va: u64) -> Result<Message, Error> {
    let mut bytes = [0u8; MESSAGE_BYTES];
    let ok = address_space(k, thread).is_some_and(|a| a.read(va, &mut bytes, Access::UserRead));
    if !ok {
        return Err(Error::InvalidArgument);
    }
    let message = ipc::from_bytes(&bytes);
    ipc::validate(&message)?;
    Ok(message)
}

// ---------------------------------------------------------------------------------------------
// IPC

fn caps_error(e: CSpaceError) -> Error {
    match e {
        CSpaceError::MissingAuthority | CSpaceError::RightsNotSubset => Error::PermissionDenied,
        CSpaceError::OccupiedSlot => Error::InvalidOperation,
        CSpaceError::NoSuchSpace | CSpaceError::SlotOutOfRange | CSpaceError::EmptySlot => {
            Error::InvalidCapability
        }
    }
}

/// Moves `message` from `from` to `to`: transfers its capabilities (each needs GRANT and lands
/// in the receiver's first empty slots, as a child of the sender's capability so the sender can
/// revoke it) and writes it into `to`'s buffer. Nothing changes on error.
fn deliver(
    k: &mut Kernel,
    from: ThreadId,
    to: ThreadId,
    mut message: Message,
    buffer: u64,
) -> Result<(), Error> {
    let from_space = k.sched.thread(from).and_then(|t| t.cspace);
    let to_space = k.sched.thread(to).and_then(|t| t.cspace);
    let mut installed: Vec<usize> = Vec::new();
    let mut result = Ok(());
    for i in 0..message.info.caps() {
        let (Some(fs), Some(ts)) = (from_space, to_space) else {
            result = Err(Error::InvalidCapability);
            break;
        };
        let source = usize::try_from(message.caps[i]).unwrap_or(usize::MAX);
        let step = k.caps.get(fs, source).map_err(caps_error).and_then(|c| {
            let dest = k.caps.first_empty_slot(ts).ok_or(Error::InvalidOperation)?;
            k.caps
                .transfer((fs, source), (ts, dest), c.rights())
                .map_err(caps_error)?;
            Ok(dest)
        });
        match step {
            Ok(dest) => {
                installed.push(dest);
                message.caps[i] = dest as u64;
            }
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    if result.is_ok() {
        let written = address_space(k, to)
            .is_some_and(|a| a.write(buffer, ipc::as_bytes(&message), Access::UserWrite));
        if !written {
            result = Err(Error::InvalidArgument);
        }
    }
    if result.is_err()
        && let Some(ts) = to_space
    {
        for slot in installed {
            let _ = k.caps.delete(ts, slot);
        }
    }
    result
}

fn send(k: &mut Kernel, slot: u64, buffer: u64, call: bool) -> Step {
    let c = cap(k, slot, Rights::WRITE)?;
    let ep = endpoint_cap(k, slot, Rights::WRITE)?;
    let cur = k.sched.current();
    let message = read_message(k, cur, buffer)?;
    if message.info.caps() > 0 && !c.rights().contains(Rights::GRANT) {
        return Err(Error::PermissionDenied);
    }
    let badge = c.badge();
    if let Some(receiver) = endpoint(k, ep).receivers.pop_front() {
        let rbuf = k.sched.thread(receiver).map_or(0, |t| t.ipc.buffer);
        if let Err(e) = deliver(k, cur, receiver, message, rbuf) {
            endpoint(k, ep).receivers.push_front(receiver);
            return Err(e);
        }
        k.sched.wake(receiver, [0, badge, message.label]);
        if call {
            if let Some(r) = k.sched.thread_mut(receiver) {
                r.reply_to = Some(cur);
            }
            k.sched.current_tcb().ipc.buffer = buffer;
            k.sched.block_current(Wait::Reply(receiver));
            return Ok(Outcome::Blocked);
        }
        return Ok(Outcome::Done(0, 0));
    }
    k.sched.current_tcb().ipc = Ipc {
        message,
        badge,
        call,
        buffer,
    };
    endpoint(k, ep).senders.push_back(cur);
    k.sched.block_current(Wait::Send(ep));
    Ok(Outcome::Blocked)
}

/// Receives on `ep` into `buffer` (already checked), blocking if no sender waits.
fn recv(k: &mut Kernel, ep: ObjectId, buffer: u64) -> Outcome {
    let cur = k.sched.current();
    while let Some(sender) = endpoint(k, ep).senders.pop_front() {
        let Some(ipc) = k.sched.thread(sender).map(|t| t.ipc) else {
            continue;
        };
        match deliver(k, sender, cur, ipc.message, buffer) {
            Ok(()) => {
                if ipc.call {
                    if let Some(s) = k.sched.thread_mut(sender) {
                        s.state = State::Blocked(Wait::Reply(cur));
                    }
                    k.sched.current_tcb().reply_to = Some(sender);
                } else {
                    k.sched.wake(sender, [0; 3]);
                }
                return Outcome::Done(ipc.badge, ipc.message.label);
            }
            Err(e) => {
                k.sched.wake(sender, [e.raw(), 0, 0]);
            }
        }
    }
    k.sched.current_tcb().ipc.buffer = buffer;
    endpoint(k, ep).receivers.push_back(cur);
    k.sched.block_current(Wait::Recv(ep));
    Outcome::Blocked
}

fn reply_recv(k: &mut Kernel, slot: u64, buffer: u64) -> Step {
    let ep = endpoint_cap(k, slot, Rights::READ)?;
    check_buffer(k, buffer)?;
    let cur = k.sched.current();
    if let Some(caller) = k.sched.current_tcb().reply_to {
        let message = read_message(k, cur, buffer)?;
        k.sched.current_tcb().reply_to = None;
        if k.sched.is_blocked_on(caller, Wait::Reply(cur)) {
            let cbuf = k.sched.thread(caller).map_or(0, |t| t.ipc.buffer);
            let result = deliver(k, cur, caller, message, cbuf);
            k.sched
                .wake(caller, status(result.map(|()| (0, message.label))));
        }
    }
    Ok(recv(k, ep, buffer))
}

fn signal(k: &mut Kernel, n: ObjectId) {
    if let Some(Object::Notification(note)) = k.objects.get_mut(n)
        && let Some((waiter, count)) = note.signal()
    {
        k.sched.wake(waiter, [0, count, 0]);
    }
}

/// Called by the I/O APIC vector of `gsi` (after EOI): signals the notification bound to it.
pub fn irq_fired(gsi: u8) {
    objects::try_with(|k| {
        let bound = k.objects.ids().find_map(|(_, o)| match o {
            Object::Irq(IrqLine {
                gsi: g,
                notification: Some(n),
            }) if *g == gsi => Some(*n),
            _ => None,
        });
        if let Some(n) = bound {
            signal(k, n);
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Threads

/// Stops `id` for good: takes it off every wait queue, fails callers waiting for its reply,
/// frees its CSpace if no other thread shares it, and removes it from the scheduler.
pub fn kill_thread(k: &mut Kernel, id: ThreadId, why: Exit) {
    let Some(tcb) = k.sched.thread(id) else {
        return;
    };
    if tcb.state == State::Dead {
        return;
    }
    let cspace = tcb.cspace;
    match tcb.state {
        State::Blocked(Wait::Send(ep) | Wait::Recv(ep)) => {
            if let Some(Object::Endpoint(e)) = k.objects.get_mut(ep) {
                e.remove(id);
            }
        }
        State::Blocked(Wait::Notification(n)) => {
            if let Some(Object::Notification(note)) = k.objects.get_mut(n) {
                note.waiters.retain(|t| *t != id);
            }
        }
        _ => {}
    }
    let callers: Vec<ThreadId> = k
        .sched
        .threads()
        .filter(|t| t.state == State::Blocked(Wait::Reply(id)))
        .map(|t| t.id)
        .collect();
    for c in callers {
        k.sched.wake(c, [Error::Closed.raw(), 0, 0]);
    }
    k.sched.kill(id, why);
    if let Some(space) = cspace
        && !k
            .sched
            .threads()
            .any(|t| t.id != id && t.state != State::Dead && t.cspace == Some(space))
    {
        let _ = k.caps.destroy_space(space);
    }
}

/// Kills the running user thread after a CPU exception in ring 3 and runs something else.
pub fn user_fault(what: core::fmt::Arguments<'_>, rip: u64) -> ! {
    let id = objects::with(|k| {
        let id = k.sched.current();
        kill_thread(k, id, Exit::Faulted);
        id
    });
    kprintln!("chisel: thread {id} killed: {what} at {rip:#x}");
    loop {
        scheduler::reschedule();
    }
}

// ---------------------------------------------------------------------------------------------
// Objects

/// Destroys `id`: tears down its kernel state, deletes every capability to it and credits its
/// budget. Refuses the root budget, budgets still paying for objects, and address spaces a live
/// thread runs in.
pub fn destroy_object(k: &mut Kernel, id: ObjectId) -> Result<(), Error> {
    if id == k.objects.root_budget() {
        return Err(Error::InvalidOperation);
    }
    match k.objects.get(id).ok_or(Error::InvalidCapability)? {
        Object::Budget(_) if k.objects.pays_for_anything(id) => {
            return Err(ObjectError::InUse.into());
        }
        Object::AddressSpace(_)
            if k.sched
                .threads()
                .any(|t| t.space == Some(id) && t.state != State::Dead) =>
        {
            return Err(ObjectError::InUse.into());
        }
        Object::Thread => kill_thread(k, id, Exit::Destroyed),
        Object::Irq(line) => ioapic::set_masked(line.gsi, true),
        Object::Notification(_) => {
            let lines: Vec<ObjectId> = k
                .objects
                .ids()
                .filter(|(_, o)| matches!(o, Object::Irq(l) if l.notification == Some(id)))
                .map(|(i, _)| i)
                .collect();
            for line in lines {
                if let Some(Object::Irq(l)) = k.objects.get_mut(line) {
                    l.notification = None;
                    ioapic::set_masked(l.gsi, true);
                }
            }
        }
        _ => {}
    }
    let object = k.objects.remove(id)?;
    k.caps.delete_object(id);
    let waiters: Vec<ThreadId> = match &object {
        Object::Endpoint(e) => e.senders.iter().chain(&e.receivers).copied().collect(),
        Object::Notification(n) => n.waiters.iter().copied().collect(),
        _ => Vec::new(),
    };
    for t in waiters {
        k.sched.wake(t, [Error::Closed.raw(), 0, 0]);
    }
    Ok(())
}

/// Destroys everything `budget` pays for (child budgets first, recursively), then the budget.
#[cfg_attr(not(test), allow(dead_code))]
pub fn reclaim_budget(k: &mut Kernel, budget: ObjectId) -> Result<(), Error> {
    loop {
        let next = k
            .objects
            .ids()
            .find(|(i, _)| *i != budget && k.objects.charge_of(*i).map(|c| c.0) == Some(budget))
            .map(|(i, o)| (i, matches!(o, Object::Budget(_))));
        match next {
            Some((child, true)) => reclaim_budget(k, child)?,
            Some((object, false)) => {
                // Threads first so no live thread pins an address space.
                let thread = k
                    .objects
                    .ids()
                    .find(|(i, o)| {
                        matches!(o, Object::Thread)
                            && k.objects.charge_of(*i).map(|c| c.0) == Some(budget)
                    })
                    .map(|(i, _)| i);
                destroy_object(k, thread.unwrap_or(object))?;
            }
            None => break,
        }
    }
    destroy_object(k, budget)
}

fn now_ns() -> u64 {
    scheduler::now_ticks() * TICK_NS
}

fn map_error(e: MapUserError) -> Error {
    match e {
        MapUserError::BadAddress | MapUserError::WritableExecutable => Error::InvalidArgument,
        MapUserError::AlreadyMapped => Error::InvalidOperation,
        MapUserError::OutOfMemory => Error::OutOfMemory,
    }
}

fn invoke(k: &mut Kernel, slot: u64, m: u64, a: [u64; 4]) -> Step {
    let space = current_space(k)?;
    match m {
        method::DESCRIBE => {
            let c = cap(k, slot, Rights::default())?;
            let ty = k
                .objects
                .get(c.object())
                .ok_or(Error::InvalidCapability)?
                .object_type();
            Ok(Outcome::Done(ty as u64, u64::from(c.rights().bits())))
        }
        method::DESTROY => {
            let c = cap(k, slot, Rights::WRITE)?;
            destroy_object(k, c.object())?;
            if k.sched
                .thread(k.sched.current())
                .is_some_and(|t| t.state == State::Dead)
            {
                return Ok(Outcome::Exited);
            }
            Ok(Outcome::Done(0, 0))
        }
        method::BUDGET_READ => {
            let b = typed_cap(k, slot, Rights::READ, ObjectType::Budget)?.object();
            let budget = k.objects.budget_mut(b).ok_or(Error::InvalidCapability)?;
            Ok(Outcome::Done(
                budget.memory_used_bytes(),
                budget.limits().memory_bytes(),
            ))
        }
        method::BUDGET_CREATE => {
            let b = typed_cap(k, slot, Rights::WRITE, ObjectType::Budget)?.object();
            let ty = ObjectType::from_raw(a[0]).ok_or(Error::InvalidArgument)?;
            let dest = empty_slot(k, space, a[1])?;
            let id = match ty {
                ObjectType::Budget => k.objects.create_budget(b, a[2], a[3], now_ns())?,
                ObjectType::Thread => create_thread(k, b, space)?,
                ObjectType::Irq | ObjectType::IrqControl => return Err(Error::InvalidArgument),
                _ => k.objects.create(ty, b)?,
            };
            k.caps
                .insert(space, dest, id, Rights::ALL, 0)
                .map_err(caps_error)?;
            Ok(Outcome::Done(dest as u64, 0))
        }
        method::THREAD_START => {
            let t = typed_cap(k, slot, Rights::WRITE, ObjectType::Thread)?.object();
            let (rip, rsp, rdi) = (a[0], a[1], a[2]);
            if rip >= carv_abi::USER_TOP || rsp > carv_abi::USER_TOP {
                return Err(Error::InvalidArgument);
            }
            let as_id = if a[3] == u64::MAX {
                k.sched.current_tcb().space.ok_or(Error::InvalidOperation)?
            } else {
                typed_cap(k, a[3], Rights::WRITE, ObjectType::AddressSpace)?.object()
            };
            let Some(Object::AddressSpace(aspace)) = k.objects.get(as_id) else {
                return Err(Error::InvalidCapability);
            };
            let root = aspace.root();
            let mut entry = UserContext::new(rip, rsp);
            entry.rdi = rdi;
            if !k.sched.start(t, as_id, root, entry) {
                return Err(Error::InvalidOperation);
            }
            Ok(Outcome::Done(0, 0))
        }
        method::ADDRESS_SPACE_MAP => {
            let s = typed_cap(k, slot, Rights::WRITE, ObjectType::AddressSpace)?.object();
            let (va, flags) = (a[0], a[1]);
            if flags & !(map::WRITE | map::EXEC) != 0 {
                return Err(Error::InvalidArgument);
            }
            map_page(k, s, va, flags & map::WRITE != 0, flags & map::EXEC != 0)?;
            Ok(Outcome::Done(0, 0))
        }
        method::IRQ_CONTROL_GET => {
            typed_cap(k, slot, Rights::WRITE, ObjectType::IrqControl)?;
            let gsi = u8::try_from(a[0]).map_err(|_| Error::InvalidArgument)?;
            if gsi >= ioapic::MAX_GSI || !ioapic::has_input(gsi) {
                return Err(Error::InvalidArgument);
            }
            let claimed = k
                .objects
                .ids()
                .any(|(_, o)| matches!(o, Object::Irq(l) if l.gsi == gsi));
            if claimed {
                return Err(Error::InvalidOperation);
            }
            let dest = empty_slot(k, space, a[1])?;
            // The Irq object is paid for by the budget the calling thread runs on.
            let payer = k
                .sched
                .current_tcb()
                .budget
                .unwrap_or_else(|| k.objects.root_budget());
            let line = IrqLine {
                gsi,
                notification: None,
            };
            let id = k
                .objects
                .insert(payer, Object::Irq(line), object_bytes(ObjectType::Irq))?;
            k.caps
                .insert(space, dest, id, Rights::ALL, 0)
                .map_err(caps_error)?;
            Ok(Outcome::Done(dest as u64, 0))
        }
        method::IRQ_BIND | method::IRQ_UNBIND => {
            let irq = typed_cap(k, slot, Rights::WRITE, ObjectType::Irq)?.object();
            let notification = if m == method::IRQ_BIND {
                Some(typed_cap(k, a[0], Rights::WRITE, ObjectType::Notification)?.object())
            } else {
                None
            };
            let Some(Object::Irq(line)) = k.objects.get_mut(irq) else {
                return Err(Error::InvalidCapability);
            };
            line.notification = notification;
            ioapic::set_masked(line.gsi, notification.is_none());
            Ok(Outcome::Done(0, 0))
        }
        _ => Err(Error::InvalidOperation),
    }
}

fn create_thread(k: &mut Kernel, budget: ObjectId, space: SpaceId) -> Result<ObjectId, Error> {
    let id = k
        .objects
        .insert(budget, Object::Thread, object_bytes(ObjectType::Thread))?;
    if k.sched.create_user_thread(id, budget, space).is_none() {
        let _ = k.objects.remove(id);
        return Err(Error::OutOfMemory);
    }
    Ok(id)
}

/// Maps one zeroed page at `va` into address-space object `space`, charging its budget.
pub fn map_page(
    k: &mut Kernel,
    space: ObjectId,
    va: u64,
    write: bool,
    exec: bool,
) -> Result<(), Error> {
    k.objects.charge_more(space, objects::PAGE_BYTES)?;
    let result = match k.objects.get_mut(space) {
        Some(Object::AddressSpace(a)) => a.map_zeroed(va, write, exec).map_err(map_error),
        _ => Err(Error::InvalidCapability),
    };
    if result.is_err() {
        k.objects.uncharge(space, objects::PAGE_BYTES);
    }
    result
}

// ---------------------------------------------------------------------------------------------
// Building processes from the kernel (init, tests)

/// A user process built by the kernel: one thread, its address space and CSpace.
#[derive(Clone, Copy, Debug)]
pub struct Process {
    /// The thread object (also the thread id).
    pub thread: ObjectId,
    /// The address-space object.
    pub space: ObjectId,
    /// The CSpace the thread names capabilities in.
    #[cfg_attr(not(test), allow(dead_code))]
    pub cspace: SpaceId,
}

/// Creates an empty process paid for by `budget`, with the initial CSpace layout of
/// [`carv_abi::init`]: the budget, its own thread and address space, and (if `irq_control`)
/// the IRQ-control capability.
pub fn create_process(
    k: &mut Kernel,
    budget: ObjectId,
    irq_control: bool,
) -> Result<Process, Error> {
    let space = k.objects.create(ObjectType::AddressSpace, budget)?;
    let cspace = k.caps.create_space(init::CSPACE_SLOTS);
    let thread = match create_thread(k, budget, cspace) {
        Ok(t) => t,
        Err(e) => {
            let _ = k.objects.remove(space);
            let _ = k.caps.destroy_space(cspace);
            return Err(e);
        }
    };
    let mut installs = alloc::vec![
        (init::ROOT_BUDGET, budget),
        (init::THREAD, thread),
        (init::ADDRESS_SPACE, space),
    ];
    if irq_control {
        let existing = k
            .objects
            .ids()
            .find(|(_, o)| matches!(o, Object::IrqControl))
            .map(|(i, _)| i);
        let control = match existing {
            Some(c) => c,
            None => {
                let root = k.objects.root_budget();
                k.objects.insert(
                    root,
                    Object::IrqControl,
                    object_bytes(ObjectType::IrqControl),
                )?
            }
        };
        installs.push((init::IRQ_CONTROL, control));
    }
    for (slot, object) in installs {
        k.caps
            .insert(cspace, slot as usize, object, Rights::ALL, 0)
            .map_err(caps_error)?;
    }
    Ok(Process {
        thread,
        space,
        cspace,
    })
}

/// Maps `bytes` at page-aligned `va` into `space` (fresh pages with the given permissions) and
/// copies them in.
#[cfg_attr(not(test), allow(dead_code))]
pub fn load(
    k: &mut Kernel,
    space: ObjectId,
    va: u64,
    bytes: &[u8],
    write: bool,
    exec: bool,
) -> Result<(), Error> {
    let pages = bytes.len().div_ceil(4096).max(1) as u64;
    for i in 0..pages {
        map_page(k, space, va + i * 4096, write, exec)?;
    }
    match k.objects.get_mut(space) {
        Some(Object::AddressSpace(a)) => {
            if a.write(va, bytes, Access::Kernel) {
                Ok(())
            } else {
                Err(Error::InvalidArgument)
            }
        }
        _ => Err(Error::InvalidCapability),
    }
}

/// Validates an ELF image, maps its segments into `space` (charging every page) and returns
/// its entry point.
pub fn load_elf(k: &mut Kernel, space: ObjectId, bytes: &[u8]) -> Result<u64, Error> {
    let mut segments = [crate::elf::LoadSegment::default(); crate::elf::MAX_SEGMENTS];
    let image = crate::elf::validate(crate::elf::Module { bytes }, &mut segments)
        .map_err(|_| Error::InvalidArgument)?;
    let Some(Object::AddressSpace(a)) = k.objects.get_mut(space) else {
        return Err(Error::InvalidCapability);
    };
    let before = a.mapped_pages();
    let entry = image.map_into(a).map_err(|_| Error::InvalidArgument);
    let pages = (a.mapped_pages() - before) as u64;
    k.objects.charge_more(space, pages * objects::PAGE_BYTES)?;
    entry
}

/// Maps the initial stack ([`carv_abi::init::STACK_PAGES`] pages below
/// [`carv_abi::init::STACK_TOP`]) and returns the stack pointer to start with.
pub fn map_stack(k: &mut Kernel, space: ObjectId) -> Result<u64, Error> {
    for i in 1..=init::STACK_PAGES {
        map_page(k, space, init::STACK_TOP - i * 4096, true, false)?;
    }
    Ok(init::STACK_TOP)
}

/// Starts the process's thread at `rip` with stack `rsp` and first argument `rdi`.
pub fn start(k: &mut Kernel, p: &Process, rip: u64, rsp: u64, rdi: u64) -> Result<(), Error> {
    let Some(Object::AddressSpace(a)) = k.objects.get(p.space) else {
        return Err(Error::InvalidCapability);
    };
    let root = a.root();
    let mut entry = UserContext::new(rip, rsp);
    entry.rdi = rdi;
    if k.sched.start(p.thread, p.space, root, entry) {
        Ok(())
    } else {
        Err(Error::InvalidOperation)
    }
}

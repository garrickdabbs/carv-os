//! Phase 2 tests that run real ring-3 code (P2.4–P2.9).
//!
//! Each test builds small user processes from the position-independent programs assembled below
//! (copied to [`CODE`], with one zeroed data page at [`DATA`] and the standard stack), starts
//! them on a budget carved out of the root budget, blocks the boot thread while they run, then
//! reads their results out of the data page and reclaims the budget (killing anything still
//! alive). The programs talk to the kernel only through the ABI syscalls.

use alloc::vec::Vec;

use carv_abi::{Error, ObjectType};
use carv_caps::Rights;

use crate::objects::{self, Object, ObjectId};
use crate::scheduler::{self, Exit, ThreadId};
use crate::syscalls::{self, Process};

/// Where test programs' code is loaded (read + execute).
const CODE: u64 = 0x40_0000;
/// The test programs' data page (read + write); results live at fixed offsets in it.
const DATA: u64 = 0x50_0000;

// Data-page offsets the programs write.
const FLAG: u64 = 0x110;
const R0: u64 = 0x118;
const R1: u64 = 0x120;
const R2: u64 = 0x128;
const R3: u64 = 0x130;

core::arch::global_asm!(
    ".pushsection .rodata.usertest, \"a\"",
    // ---- hello: print through debug_putc, then destroy our own thread.
    ".global ut_hello_start, ut_hello_end",
    "ut_hello_start:",
    "    lea rbx, [rip + 3f]",
    "1:  movzx edi, byte ptr [rbx]",
    "    test edi, edi",
    "    jz 2f",
    "    xor eax, eax",
    "    syscall",
    "    inc rbx",
    "    jmp 1b",
    "2:  mov eax, 8",
    "    mov edi, 1",
    "    mov esi, 1",
    "    syscall",
    "    ud2",
    "3:  .asciz \"user: hello from ring 3\\n\"",
    "ut_hello_end:",
    // ---- fault: read a kernel address.
    ".global ut_fault_start, ut_fault_end",
    "ut_fault_start:",
    "    mov rax, 0xffffffff80000000",
    "    mov rax, [rax]",
    "    ud2",
    "ut_fault_end:",
    // ---- spin: count forever in DATA[0].
    ".global ut_spin_start, ut_spin_end",
    "ut_spin_start:",
    "    mov eax, 0x500000",
    "1:  inc qword ptr [rax]",
    "    jmp 1b",
    "ut_spin_end:",
    // ---- fpu: keep the initial XMM0 value across every yield and context switch.
    ".global ut_fpu_start, ut_fpu_end",
    "ut_fpu_start:",
    "    mov ebx, 0x500000",
    "    movd xmm0, edi",
    "1:  mov eax, 7",
    "    syscall",
    "    movd eax, xmm0",
    "    cmp eax, edi",
    "    jne 2f",
    "    inc qword ptr [rbx + 0x118]",
    "    jmp 1b",
    "2:  mov qword ptr [rbx + 0x110], 1",
    "    jmp 2b",
    "ut_fpu_end:",
    // ---- ping client: rdi = rounds. Calls slot 4 with word0 = n, expects n + 1 back; sums the
    //      rdtsc cycles of every round trip into R1 and sets FLAG = 1 (or the error code).
    ".global ut_client_start, ut_client_end",
    "ut_client_start:",
    "    mov ebx, 0x500000",
    "    mov r12, rdi",
    "    xor r13d, r13d",
    "1:  mov [rbx], r12",
    "    mov qword ptr [rbx + 8], 1",
    "    mov [rbx + 16], r12",
    "    rdtsc",
    "    shl rdx, 32",
    "    or rax, rdx",
    "    mov r14, rax",
    "    mov eax, 3",
    "    mov edi, 4",
    "    mov rsi, rbx",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    rdtsc",
    "    shl rdx, 32",
    "    or rax, rdx",
    "    sub rax, r14",
    "    add r13, rax",
    "    mov rax, [rbx + 16]",
    "    lea rcx, [r12 + 1]",
    "    cmp rax, rcx",
    "    jne 7f",
    "    dec r12",
    "    jnz 1b",
    "    mov [rbx + 0x120], r13",
    "    mov qword ptr [rbx + 0x110], 1",
    "    jmp 9f",
    "7:  mov eax, 0x100",
    "8:  mov [rbx + 0x110], rax",
    "9:  mov eax, 8",
    "    mov edi, 1",
    "    mov esi, 1",
    "    syscall",
    "    ud2",
    "ut_client_end:",
    // ---- ping server: recv on slot 4 (badge → R0), then reply word0 + 1 forever; R1 counts
    //      replies; an error lands in FLAG.
    ".global ut_server_start, ut_server_end",
    "ut_server_start:",
    "    mov ebx, 0x500000",
    "    mov eax, 2",
    "    mov edi, 4",
    "    mov rsi, rbx",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    mov [rbx + 0x118], rdx",
    "1:  inc qword ptr [rbx + 16]",
    "    mov qword ptr [rbx + 8], 1",
    "    mov eax, 4",
    "    mov edi, 4",
    "    mov rsi, rbx",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    inc qword ptr [rbx + 0x120]",
    "    jmp 1b",
    "8:  mov [rbx + 0x110], rax",
    "    mov eax, 8",
    "    mov edi, 1",
    "    mov esi, 1",
    "    syscall",
    "    ud2",
    "ut_server_end:",
    // ---- cap sender: send slot 5 (a notification) over slot 4, then wait on slot 5.
    //      FLAG = send status, R0 = wait status, R1 = wait count.
    ".global ut_capsend_start, ut_capsend_end",
    "ut_capsend_start:",
    "    mov ebx, 0x500000",
    "    mov qword ptr [rbx], 9",
    "    mov qword ptr [rbx + 8], 0x100",
    "    mov qword ptr [rbx + 64], 5",
    "    mov eax, 1",
    "    mov edi, 4",
    "    mov rsi, rbx",
    "    syscall",
    "    mov [rbx + 0x110], rax",
    "    mov eax, 6",
    "    mov edi, 5",
    "    syscall",
    "    mov [rbx + 0x118], rax",
    "    mov [rbx + 0x120], rdx",
    "    mov eax, 8",
    "    mov edi, 1",
    "    mov esi, 1",
    "    syscall",
    "    ud2",
    "ut_capsend_end:",
    // ---- cap receiver: recv on slot 4, signal the transferred cap twice.
    //      FLAG = recv status, R0 = label, R2 = the slot the cap arrived in, R3 = signal status.
    ".global ut_caprecv_start, ut_caprecv_end",
    "ut_caprecv_start:",
    "    mov ebx, 0x500000",
    "    mov eax, 2",
    "    mov edi, 4",
    "    mov rsi, rbx",
    "    syscall",
    "    mov [rbx + 0x110], rax",
    "    mov rax, [rbx]",
    "    mov [rbx + 0x118], rax",
    "    mov rdi, [rbx + 64]",
    "    mov [rbx + 0x128], rdi",
    "    mov r12, rdi",
    "    mov eax, 5",
    "    syscall",
    "    mov rdi, r12",
    "    mov eax, 5",
    "    syscall",
    "    mov [rbx + 0x130], rax",
    "1:  mov eax, 7",
    "    syscall",
    "    jmp 1b",
    "ut_caprecv_end:",
    // ---- irq waiter: rdi = GSI. Creates a notification (slot 4) from the budget, gets the
    //      Irq for the GSI from IRQ control (slot 3 → slot 5), binds it, waits five times
    //      (R0 = wakeups, R1 = summed counts), unbinds. FLAG = 1, or 0x1000 * step + error.
    ".global ut_irq_start, ut_irq_end",
    "ut_irq_start:",
    "    mov ebx, 0x500000",
    "    mov r15, rdi",
    "    mov r14d, 1",
    "    mov eax, 8",
    "    xor edi, edi",
    "    mov esi, 3",
    "    mov edx, 5",
    "    mov r10d, 4",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    mov r14d, 2",
    "    mov eax, 8",
    "    mov edi, 3",
    "    mov esi, 6",
    "    mov rdx, r15",
    "    mov r10d, 5",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    mov r14d, 3",
    "    mov eax, 8",
    "    mov edi, 5",
    "    mov esi, 7",
    "    mov edx, 4",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    mov r14d, 4",
    "    mov r12d, 5",
    "1:  mov eax, 6",
    "    mov edi, 4",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    inc qword ptr [rbx + 0x118]",
    "    add [rbx + 0x120], rdx",
    "    dec r12",
    "    jnz 1b",
    "    mov r14d, 5",
    "    mov eax, 8",
    "    mov edi, 5",
    "    mov esi, 8",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    mov qword ptr [rbx + 0x110], 1",
    "    jmp 9f",
    "8:  shl r14, 12",
    "    or rax, r14",
    "    mov [rbx + 0x110], rax",
    "9:  mov eax, 8",
    "    mov edi, 1",
    "    mov esi, 1",
    "    syscall",
    "    ud2",
    "ut_irq_end:",
    // ---- objects: create one object of every creatable type from the budget (slots 4..=10),
    //      DESCRIBE each (types → DATA[0x300..]), read the budget's memory use before (R0),
    //      with everything alive (R1) and after destroying it all (R2); check a few refusals
    //      (0x200: bad slot, 0x208: kernel buffer, 0x210: Irq from a budget, 0x218: an
    //      oversized child budget). FLAG = 1, or 0x1000 * step + error.
    ".global ut_objects_start, ut_objects_end",
    "ut_objects_start:",
    "    mov ebx, 0x500000",
    "    mov eax, 8",
    "    xor edi, edi",
    "    mov esi, 2",
    "    syscall",
    "    mov [rbx + 0x118], rdx",
    "    lea r12, [rip + 5f]",
    "    xor r13d, r13d",
    "1:  mov r14, r13",
    "    or r14, 0x10",
    "    movzx edx, byte ptr [r12 + r13]",
    "    mov eax, 8",
    "    xor edi, edi",
    "    mov esi, 3",
    "    lea r10, [r13 + 4]",
    "    xor r8d, r8d",
    "    mov r9d, 4096",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    or r14, 0x20",
    "    mov eax, 8",
    "    lea rdi, [r13 + 4]",
    "    xor esi, esi",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    mov [rbx + 0x300 + r13 * 8], rdx",
    "    inc r13",
    "    cmp r13, 7",
    "    jne 1b",
    "    mov eax, 8",
    "    xor edi, edi",
    "    mov esi, 2",
    "    syscall",
    "    mov [rbx + 0x120], rdx",
    // refusals
    "    mov eax, 1",
    "    mov edi, 60",
    "    mov rsi, rbx",
    "    syscall",
    "    mov [rbx + 0x200], rax",
    "    mov eax, 2",
    "    mov edi, 6",
    "    mov rsi, 0xffff800000000000",
    "    syscall",
    "    mov [rbx + 0x208], rax",
    "    mov eax, 8",
    "    xor edi, edi",
    "    mov esi, 3",
    "    mov edx, 8",
    "    mov r10d, 20",
    "    syscall",
    "    mov [rbx + 0x210], rax",
    "    mov eax, 8",
    "    xor edi, edi",
    "    mov esi, 3",
    "    mov edx, 6",
    "    mov r10d, 20",
    "    xor r8d, r8d",
    "    mov r9, 0x10000000000",
    "    syscall",
    "    mov [rbx + 0x218], rax",
    // destroy everything we created
    "    xor r13d, r13d",
    "2:  mov r14, r13",
    "    or r14, 0x40",
    "    mov eax, 8",
    "    lea rdi, [r13 + 4]",
    "    mov esi, 1",
    "    syscall",
    "    test rax, rax",
    "    jnz 8f",
    "    inc r13",
    "    cmp r13, 7",
    "    jne 2b",
    "    mov eax, 8",
    "    xor edi, edi",
    "    mov esi, 2",
    "    syscall",
    "    mov [rbx + 0x128], rdx",
    "    mov qword ptr [rbx + 0x110], 1",
    "    jmp 9f",
    "8:  shl r14, 12",
    "    or rax, r14",
    "    mov [rbx + 0x110], rax",
    "9:  mov eax, 8",
    "    mov edi, 1",
    "    mov esi, 1",
    "    syscall",
    "    ud2",
    // AddressSpace, Frame, Endpoint, Notification, Reply, Budget, Thread
    "5:  .byte 2, 3, 4, 5, 7, 6, 1",
    "ut_objects_end:",
    ".popsection",
);

unsafe extern "C" {
    static ut_hello_start: u8;
    static ut_hello_end: u8;
    static ut_fault_start: u8;
    static ut_fault_end: u8;
    static ut_spin_start: u8;
    static ut_spin_end: u8;
    static ut_fpu_start: u8;
    static ut_fpu_end: u8;
    static ut_client_start: u8;
    static ut_client_end: u8;
    static ut_server_start: u8;
    static ut_server_end: u8;
    static ut_capsend_start: u8;
    static ut_capsend_end: u8;
    static ut_caprecv_start: u8;
    static ut_caprecv_end: u8;
    static ut_irq_start: u8;
    static ut_irq_end: u8;
    static ut_objects_start: u8;
    static ut_objects_end: u8;
}

/// The bytes between two labels of the blob above.
fn program(start: &'static u8, end: &'static u8) -> &'static [u8] {
    let (s, e) = (core::ptr::from_ref(start), core::ptr::from_ref(end));
    // SAFETY: both labels delimit one contiguous run of bytes in `.rodata.usertest`, end after
    // start, and the section is never written.
    unsafe { core::slice::from_raw_parts(s, e.offset_from(s) as usize) }
}

macro_rules! prog {
    ($start:ident, $end:ident) => {
        // SAFETY: taking the addresses of two linker labels; nothing is read through them here.
        unsafe { program(&$start, &$end) }
    };
}

/// Sleeps until the CPU period boundary so a fresh period starts with the full allowance.
fn align_to_period() {
    let period = objects::CPU_PERIOD_NS / scheduler::TICK_NS;
    let now = scheduler::now_ticks();
    scheduler::sleep_ms(period - now % period + 1);
}

/// Carves a test budget of `cpu_ms` per period and 2 MiB out of the root budget, at the start of
/// a fresh period (CPU returned by earlier tests' budgets only comes back at a refill).
fn budget(cpu_ms: u64) -> ObjectId {
    align_to_period();
    budget_now(cpu_ms)
}

fn budget_now(cpu_ms: u64) -> ObjectId {
    objects::with(|k| {
        let root = k.objects.root_budget();
        k.objects
            .create_budget(
                root,
                cpu_ms * 1_000_000,
                2 << 20,
                scheduler::now_ticks() * scheduler::TICK_NS,
            )
            .expect("carving a test budget")
    })
}

/// Builds and starts a process running `code`, with first argument `rdi`.
fn spawn(budget: ObjectId, code: &[u8], rdi: u64, irq_control: bool) -> Process {
    objects::with(|k| {
        let p = syscalls::create_process(k, budget, irq_control)?;
        syscalls::load(k, p.space, CODE, code, false, true)?;
        syscalls::load(k, p.space, DATA, &[0u8; 4096], true, false)?;
        let rsp = syscalls::map_stack(k, p.space)?;
        syscalls::start(k, &p, CODE, rsp, rdi)?;
        Ok::<_, Error>(p)
    })
    .expect("spawning a test process")
}

/// A word of `p`'s data page.
fn data(p: &Process, offset: u64) -> u64 {
    objects::with(|k| match k.objects.get(p.space) {
        Some(Object::AddressSpace(a)) => a.read_u64(DATA + offset).expect("data page mapped"),
        _ => panic!("process address space gone"),
    })
}

fn exit_of(thread: ThreadId) -> Option<Exit> {
    objects::with(|k| k.sched.exit_of(thread))
}

/// Blocks the boot thread until `done` holds, checking every few ticks, for at most `ms`.
fn wait_until(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = scheduler::now_ticks() + ms;
    while scheduler::now_ticks() < deadline {
        if done() {
            return true;
        }
        scheduler::sleep_ms(2);
    }
    done()
}

/// Destroys the budget and everything it pays for.
fn reclaim(budget: ObjectId) {
    objects::with(|k| syscalls::reclaim_budget(k, budget)).expect("reclaiming a test budget");
}

#[test_case]
fn ring3_thread_prints_and_destroys_itself() {
    let b = budget(5);
    let switches = objects::with(|k| k.sched.switches());
    let p = spawn(b, prog!(ut_hello_start, ut_hello_end), 0, false);
    assert!(wait_until(500, || exit_of(p.thread) == Some(Exit::Destroyed)));
    assert!(objects::with(|k| k.sched.switches()) > switches);
    assert!(
        objects::with(|k| k.objects.get(p.thread).is_none()),
        "DESTROY removed the thread object"
    );
    reclaim(b);
}

#[test_case]
fn ring3_fault_kills_only_the_faulting_thread() {
    let b = budget(5);
    let p = spawn(b, prog!(ut_fault_start, ut_fault_end), 0, false);
    assert!(wait_until(500, || exit_of(p.thread) == Some(Exit::Faulted)));
    reclaim(b);
}

#[test_case]
fn ring3_threads_keep_independent_simd_state() {
    let b = budget(5);
    let code = prog!(ut_fpu_start, ut_fpu_end);
    let (one, two) = (
        spawn(b, code, 0x3f80_0000, false),
        spawn(b, code, 0x4000_0000, false),
    );
    assert!(wait_until(300, || data(&one, R0) > 100 && data(&two, R0) > 100));
    assert_eq!(data(&one, FLAG), 0, "first thread's XMM state changed");
    assert_eq!(data(&two, FLAG), 0, "second thread's XMM state changed");
    reclaim(b);
}

#[test_case]
fn budgets_split_the_cpu_thirty_seventy() {
    let b3 = budget(3);
    let b7 = budget_now(7);
    let spin = prog!(ut_spin_start, ut_spin_end);
    let (p3, p7) = (spawn(b3, spin, 0, false), spawn(b7, spin, 0, false));
    scheduler::sleep_ms(400);
    let (t3, t7) = objects::with(|k| {
        let ticks = |t: ThreadId| k.sched.thread(t).map_or(0, |t| t.run_ticks);
        (ticks(p3.thread), ticks(p7.thread))
    });
    let (c3, c7) = (data(&p3, 0), data(&p7, 0));
    reclaim(b3);
    reclaim(b7);
    let share = t3 * 1000 / (t3 + t7);
    kprint_share(t3, t7, c3, c7);
    assert!(t3 + t7 > 300, "spinners ran {t3} + {t7} ticks of 400 ms");
    assert!(
        (250..=350).contains(&share),
        "3 ms budget got {share}‰ of the CPU (ticks {t3}/{t7})"
    );
}

fn kprint_share(t3: u64, t7: u64, c3: u64, c7: u64) {
    crate::kprint!(
        "(ticks {t3}/{t7} = {}%, loop counts {}%) ",
        t3 * 100 / (t3 + t7),
        c3 * 100 / (c3 + c7).max(1)
    );
}

#[test_case]
fn ipc_call_reply_ping_pong() {
    const ROUNDS: u64 = 2000;
    let b = budget(5);
    let client = spawn(b, prog!(ut_client_start, ut_client_end), ROUNDS, false);
    let server = spawn(b, prog!(ut_server_start, ut_server_end), 0, false);
    objects::with(|k| {
        let ep = k.objects.create(ObjectType::Endpoint, b).unwrap();
        k.caps
            .insert(server.cspace, 4, ep, Rights::READ | Rights::WRITE, 0)
            .unwrap();
        k.caps
            .insert(client.cspace, 4, ep, Rights::WRITE, 0x77)
            .unwrap();
    });
    assert!(wait_until(5000, || data(&client, FLAG) != 0));
    assert_eq!(
        data(&client, FLAG),
        1,
        "client error {:#x}",
        data(&client, FLAG)
    );
    assert_eq!(data(&server, FLAG), 0, "server error");
    assert_eq!(data(&server, R0), 0x77, "server saw the client's badge");
    assert_eq!(data(&server, R1), ROUNDS - 1);
    let cycles = data(&client, R1) / ROUNDS;
    crate::kprint!("({ROUNDS} round trips, {cycles} TSC cycles each) ");
    reclaim(b);
}

#[test_case]
fn ipc_transfers_a_capability_and_revoke_takes_it_back() {
    let b = budget(5);
    let sender = spawn(b, prog!(ut_capsend_start, ut_capsend_end), 0, false);
    let receiver = spawn(b, prog!(ut_caprecv_start, ut_caprecv_end), 0, false);
    objects::with(|k| {
        let ep = k.objects.create(ObjectType::Endpoint, b).unwrap();
        let n = k.objects.create(ObjectType::Notification, b).unwrap();
        k.caps
            .insert(receiver.cspace, 4, ep, Rights::READ, 0)
            .unwrap();
        k.caps
            .insert(sender.cspace, 4, ep, Rights::WRITE, 0)
            .unwrap();
        k.caps.insert(sender.cspace, 5, n, Rights::ALL, 0).unwrap();
    });
    assert!(wait_until(1000, || exit_of(sender.thread).is_some()));
    assert_eq!(data(&sender, FLAG), 0, "send status");
    assert_eq!(data(&receiver, FLAG), 0, "recv status");
    assert_eq!(data(&receiver, R0), 9, "label");
    assert_eq!(data(&receiver, R3), 0, "signal through the transferred cap");
    assert_eq!(data(&sender, R0), 0, "wait status");
    assert!(data(&sender, R1) >= 1, "wait saw the signals");
    let slot = data(&receiver, R2);
    assert_eq!(
        slot,
        carv_abi::init::IRQ_CONTROL,
        "the cap landed in the receiver's first empty slot (no IRQ control here)"
    );
    reclaim(b);
}

#[test_case]
fn revoke_removes_capabilities_transferred_to_another_cspace() {
    let b = budget(5);
    let receiver = spawn(b, prog!(ut_caprecv_start, ut_caprecv_end), 0, false);
    // A second CSpace that outlives the exchange: the kernel stands in for the sender.
    objects::with(|k| {
        let ep = k.objects.create(ObjectType::Endpoint, b).unwrap();
        k.caps
            .insert(receiver.cspace, 4, ep, Rights::READ, 0)
            .unwrap();
    });
    let owner = objects::with(|k| {
        let space = k.caps.create_space(8);
        let n = k.objects.create(ObjectType::Notification, b).unwrap();
        k.caps.insert(space, 0, n, Rights::ALL, 0).unwrap();
        k.caps
            .transfer((space, 0), (receiver.cspace, 6), Rights::WRITE)
            .unwrap();
        space
    });
    objects::with(|k| {
        assert!(k.caps.get(receiver.cspace, 6).is_ok());
        k.caps.revoke(owner, 0).unwrap();
        assert!(
            k.caps.get(receiver.cspace, 6).is_err(),
            "revoked across CSpaces"
        );
        k.caps.destroy_space(owner).unwrap();
    });
    reclaim(b);
}

#[test_case]
fn irq_wakes_a_user_space_waiter() {
    use crate::arch::x86_64::port::Port;
    let gsi = crate::platform::acpi::summary()
        .expect("ACPI summary")
        .isa_irq(0)
        .gsi;
    let b = budget(5);
    let p = spawn(b, prog!(ut_irq_start, ut_irq_end), u64::from(gsi), true);
    // Let the waiter bind its Irq, then run PIT channel 0 at ~100 Hz (mode 2) on ISA IRQ 0.
    scheduler::sleep_ms(20);
    // SAFETY: standard 8254 programming; channel 0's output only reaches the (otherwise unused,
    // masked-until-bound) I/O APIC input the test owns.
    unsafe {
        Port::<u8>::new(0x43).write(0x34);
        Port::<u8>::new(0x40).write((11_932u16 & 0xff) as u8);
        Port::<u8>::new(0x40).write((11_932u16 >> 8) as u8);
    }
    let finished = wait_until(2000, || data(&p, FLAG) != 0);
    // SAFETY: as above; mode 0 with a count of 1 fires once more and then stays quiet.
    unsafe {
        Port::<u8>::new(0x43).write(0x30);
        Port::<u8>::new(0x40).write(1);
        Port::<u8>::new(0x40).write(0);
    }
    assert!(finished, "IRQ waiter never finished");
    assert_eq!(data(&p, FLAG), 1, "waiter error {:#x}", data(&p, FLAG));
    assert_eq!(data(&p, R0), 5, "five wakeups");
    assert!(data(&p, R1) >= 5, "at least five interrupts counted");
    reclaim(b);
    objects::with(|k| {
        assert!(
            !k.objects.ids().any(|(_, o)| matches!(o, Object::Irq(_))),
            "the Irq object was reclaimed"
        );
    });
}

#[test_case]
fn invoke_creates_and_destroys_every_object_type() {
    let b = budget(5);
    let p = spawn(b, prog!(ut_objects_start, ut_objects_end), 0, false);
    assert!(wait_until(1000, || data(&p, FLAG) != 0));
    assert_eq!(data(&p, FLAG), 1, "error {:#x}", data(&p, FLAG));
    let types: Vec<u64> = (0..7).map(|i| data(&p, 0x300 + 8 * i)).collect();
    assert_eq!(types, [2, 3, 4, 5, 7, 6, 1], "DESCRIBE reports each type");
    let expected: u64 = [
        ObjectType::AddressSpace,
        ObjectType::Frame,
        ObjectType::Endpoint,
        ObjectType::Notification,
        ObjectType::Reply,
        ObjectType::Budget,
        ObjectType::Thread,
    ]
    .iter()
    .map(|t| objects::object_bytes(*t))
    .sum();
    let (before, alive, after) = (data(&p, R0), data(&p, R1), data(&p, R2));
    assert_eq!(alive - before, expected, "every object was charged");
    assert_eq!(after, before, "every charge was credited back");
    assert_eq!(
        data(&p, 0x200),
        Error::InvalidCapability.raw(),
        "empty slot"
    );
    assert_eq!(
        data(&p, 0x208),
        Error::InvalidArgument.raw(),
        "kernel buffer"
    );
    assert_eq!(
        data(&p, 0x210),
        Error::InvalidArgument.raw(),
        "Irq from a budget"
    );
    assert_eq!(
        data(&p, 0x218),
        Error::BudgetExhausted.raw(),
        "oversized child"
    );
    reclaim(b);
}

#[test_case]
fn init_module_is_a_valid_user_image() {
    let module = crate::MODULES
        .response()
        .and_then(|r| {
            r.modules()
                .iter()
                .find(|m| m.path().ends_with("/init"))
                .copied()
        })
        .expect("the test ISO ships boot/init");
    let b = budget(1);
    let entry = objects::with(|k| {
        let space = k.objects.create(ObjectType::AddressSpace, b).unwrap();
        syscalls::load_elf(k, space, module.data())
    })
    .expect("init loads");
    assert_eq!(entry, CODE);
    reclaim(b);
}

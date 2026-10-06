//! x86-64 `syscall`/`sysret` entry, kernel-stack switching, ring-3 entry and thread context
//! switching (P2.5).
//!
//! `syscall` arrives at [`syscall_entry`] (assembly) with interrupts off (SFMASK clears IF) and the
//! user stack still loaded. The stub parks the user `rsp`, switches to the current thread's kernel
//! stack ([`set_kernel_stack`] keeps it and `TSS.RSP0` in step), saves the user registers as a
//! [`SyscallFrame`] and calls the dispatcher with it. The dispatcher writes the results into the
//! frame; the stub restores every argument register (so no kernel value leaks to user mode) and
//! returns with `sysretq`. A new thread enters ring 3 for the first time through `iretq`
//! ([`enter_user`]); [`switch_context`] swaps kernel stacks between threads.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

use super::gdt;

const RFLAGS_IF: u64 = 1 << 9;

/// Kernel stack top of the running thread, loaded by the syscall stub.
static KERNEL_RSP: AtomicU64 = AtomicU64::new(0);
/// Scratch slot for the user `rsp` between `syscall` and the switch to the kernel stack.
static USER_RSP: AtomicU64 = AtomicU64::new(0);

/// User registers saved by the syscall stub, lowest address first (the order it pushes them in
/// reverse). The dispatcher reads arguments from it and writes results back.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct SyscallFrame {
    /// Sixth argument.
    pub r9: u64,
    /// Fifth argument.
    pub r8: u64,
    /// Fourth argument.
    pub r10: u64,
    /// Third argument; second result.
    pub rdx: u64,
    /// Second argument; third result.
    pub rsi: u64,
    /// First argument.
    pub rdi: u64,
    /// Syscall number; status on return.
    pub rax: u64,
    /// User RFLAGS (saved by `syscall` in r11).
    pub rflags: u64,
    /// User return address (saved by `syscall` in rcx).
    pub rip: u64,
    /// User stack pointer.
    pub rsp: u64,
}

/// Where a new thread enters ring 3: consumed by [`enter_user`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct UserContext {
    /// Instruction pointer.
    pub rip: u64,
    /// Stack pointer.
    pub rsp: u64,
    /// User flags.
    pub rflags: u64,
    /// Value for `rdi` (first argument).
    pub rdi: u64,
    /// Value for `rsi` (second argument).
    pub rsi: u64,
    /// Code selector (ring 3), filled in by [`enter_user`]'s caller.
    pub cs: u64,
    /// Stack selector (ring 3).
    pub ss: u64,
}

impl UserContext {
    /// Creates a context suitable for entering ring 3 with interrupts enabled.
    pub const fn new(rip: u64, rsp: u64) -> Self {
        Self {
            rip,
            rsp,
            rflags: 0x2 | RFLAGS_IF,
            rdi: 0,
            rsi: 0,
            cs: 0,
            ss: 0,
        }
    }
}

core::arch::global_asm!(
    ".global carv_syscall_entry",
    "carv_syscall_entry:",
    "    mov [rip + {user_rsp}], rsp",
    "    mov rsp, [rip + {kernel_rsp}]",
    "    push qword ptr [rip + {user_rsp}]",
    "    push rcx",
    "    push r11",
    "    push rax",
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push r10",
    "    push r8",
    "    push r9",
    "    mov rdi, rsp",
    "    call {dispatch}",
    "    pop r9",
    "    pop r8",
    "    pop r10",
    "    pop rdx",
    "    pop rsi",
    "    pop rdi",
    "    pop rax",
    "    pop r11",
    "    pop rcx",
    "    pop rsp",
    "    sysretq",
    "",
    ".global carv_switch_context",
    "carv_switch_context:",
    "    push rbp",
    "    push rbx",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov [rdi], rsp",
    "    mov rsp, rsi",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbx",
    "    pop rbp",
    "    ret",
    "",
    ".global carv_thread_trampoline",
    "carv_thread_trampoline:",
    "    mov rdi, r12",
    "    call rbx",
    "    ud2",
    "",
    ".global carv_enter_user",
    "carv_enter_user:",
    "    push qword ptr [rdi + 48]",
    "    push qword ptr [rdi + 8]",
    "    push qword ptr [rdi + 16]",
    "    push qword ptr [rdi + 40]",
    "    push qword ptr [rdi + 0]",
    "    mov rsi, [rdi + 32]",
    "    mov rdi, [rdi + 24]",
    "    xor eax, eax",
    "    xor ebx, ebx",
    "    xor ecx, ecx",
    "    xor edx, edx",
    "    xor ebp, ebp",
    "    xor r8d, r8d",
    "    xor r9d, r9d",
    "    xor r10d, r10d",
    "    xor r11d, r11d",
    "    xor r12d, r12d",
    "    xor r13d, r13d",
    "    xor r14d, r14d",
    "    xor r15d, r15d",
    "    iretq",
    user_rsp = sym USER_RSP,
    kernel_rsp = sym KERNEL_RSP,
    dispatch = sym crate::syscalls::dispatch,
);

unsafe extern "C" {
    fn carv_syscall_entry();
    fn carv_switch_context(old_rsp: *mut u64, new_rsp: u64);
    fn carv_thread_trampoline();
    fn carv_enter_user(context: *const UserContext) -> !;
}

/// Programs EFER.SCE, STAR/LSTAR/SFMASK and turns on SMEP/SMAP. Call once after the GDT and IDT
/// are loaded.
pub fn init() {
    let sel = gdt::selectors();
    // `Star::write` checks the selector offsets that `syscall` and `sysretq` derive from STAR
    // (user data 8 bytes below user code, kernel data 8 bytes above kernel code), so a GDT that
    // breaks them panics here instead of faulting on the first return to ring 3 (#140).
    Star::write(sel.user_code, sel.user_data, sel.code, sel.data)
        .expect("GDT layout is incompatible with syscall/sysret");
    LStar::write(VirtAddr::new(carv_syscall_entry as *const () as u64));
    // `syscall` clears these RFLAGS bits on entry: IF keeps interrupts off until the stub is on a
    // kernel stack, DF gives kernel code the cleared direction flag the SysV ABI assumes, and AC
    // keeps SMAP enforced even if user code set it.
    SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::DIRECTION_FLAG | RFlags::ALIGNMENT_CHECK);
    // SAFETY: setting SCE only enables the `syscall`/`sysret` instructions, whose targets were
    // programmed above.
    unsafe { Efer::update(|f| f.insert(EferFlags::SYSTEM_CALL_EXTENSIONS)) };
    enable_smap_smep();
}

fn enable_smap_smep() {
    let mut cr4: u64;
    // SAFETY: CR4 is read and written on the boot CPU during early initialization; setting only
    // SMEP and SMAP preserves all other architectural controls.
    unsafe {
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        cr4 |= (1 << 20) | (1 << 21);
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));
    }
}

/// Address of the syscall entry stub (what LSTAR must hold).
#[cfg_attr(not(test), allow(dead_code))]
pub fn entry_address() -> u64 {
    carv_syscall_entry as *const () as u64
}

/// Makes `top` the kernel stack for the next ring-3 → ring-0 transition (syscall and interrupt).
pub fn set_kernel_stack(top: u64) {
    KERNEL_RSP.store(top, Ordering::Relaxed);
    gdt::set_kernel_stack(VirtAddr::new(top));
}

/// Saves the callee-saved registers and stack pointer into `*old_rsp` and resumes the context
/// saved at `new_rsp`.
///
/// # Safety
/// Interrupts must be disabled; `old_rsp` must stay valid until this context is resumed, and
/// `new_rsp` must be a value saved by this function or built by [`prepare_stack`].
pub unsafe fn switch_context(old_rsp: *mut u64, new_rsp: u64) {
    // SAFETY: forwarded from the caller.
    unsafe { carv_switch_context(old_rsp, new_rsp) }
}

/// Lays out a fresh kernel stack so that [`switch_context`] to the returned stack pointer calls
/// `entry(arg)` on it.
///
/// # Safety
/// `top` must be the 16-byte-aligned top of a mapped, writable kernel stack of at least 128 bytes
/// that nothing else uses.
pub unsafe fn prepare_stack(top: u64, entry: extern "C" fn(u64) -> !, arg: u64) -> u64 {
    // Popped by switch_context: r15, r14, r13, r12 (= arg), rbx (= entry), rbp, then the return
    // address. After `ret` the stack is 16 bytes below `top`, aligned for the trampoline's call.
    let frame: [u64; 7] = [
        0,
        0,
        0,
        arg,
        entry as usize as u64,
        0,
        carv_thread_trampoline as *const () as u64,
    ];
    let start = top - 16 - (frame.len() as u64) * 8;
    // SAFETY: the caller guarantees `[top - 128, top)` is a writable stack nobody else uses.
    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), start as *mut u64, frame.len());
    }
    start
}

/// Enters ring 3 with `context` through `iretq`, all other general registers zeroed.
///
/// # Safety
/// Interrupts must be disabled, the target address space must be active, and the current kernel
/// stack must be the one installed with [`set_kernel_stack`].
pub unsafe fn enter_user(mut context: UserContext) -> ! {
    let sel = gdt::selectors();
    context.cs = u64::from(sel.user_code.0);
    context.ss = u64::from(sel.user_data.0);
    // SAFETY: forwarded from the caller; the selectors are the ring-3 GDT entries (RPL 3).
    unsafe { carv_enter_user(&context) }
}

//! x86-64 syscall entry configuration and user context helpers.
//!
//! The MSR setup is intentionally separate from dispatch: P2.7 will add capability
//! lookup and message decoding without changing the architectural entry contract.

use x86_64::VirtAddr;
use x86_64::registers::model_specific::{LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

use super::gdt;

const RFLAGS_IF: u64 = 1 << 9;

/// Saved general-purpose and control state for a preempted thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
#[allow(dead_code)]
pub struct UserContext {
    /// Instruction pointer.
    pub rip: u64,
    /// Stack pointer.
    pub rsp: u64,
    /// User flags.
    pub rflags: u64,
    /// General-purpose argument registers, in syscall ABI order.
    pub rax: u64,
    /// First argument.
    pub rdi: u64,
    /// Second argument.
    pub rsi: u64,
    /// Third argument.
    pub rdx: u64,
    /// Fourth argument.
    pub r10: u64,
    /// Fifth argument.
    pub r8: u64,
    /// Sixth argument.
    pub r9: u64,
}

impl UserContext {
    /// Creates a context suitable for entering ring 3 with interrupts enabled.
    #[allow(dead_code)]
    pub const fn new(rip: u64, rsp: u64) -> Self {
        Self {
            rip,
            rsp,
            rflags: 0x2 | RFLAGS_IF,
            rax: 0,
            rdi: 0,
            rsi: 0,
            rdx: 0,
            r10: 0,
            r8: 0,
            r9: 0,
        }
    }
}

/// Programs STAR/LSTAR/SFMASK and turns on SMEP/SMAP. Call once after the GDT and IDT are
/// loaded.
///
/// The entry stub is a safe placeholder until the syscall dispatcher and IPC ABI land.
pub fn init() {
    let sel = gdt::selectors();
    // `Star::write` checks the selector offsets that `syscall` and `sysretq` derive from STAR
    // (user data 8 bytes below user code, kernel data 8 bytes above kernel code), so a GDT that
    // breaks them panics here instead of faulting on the first return to ring 3 (#140).
    Star::write(sel.user_code, sel.user_data, sel.code, sel.data)
        .expect("GDT layout is incompatible with syscall/sysret");
    LStar::write(VirtAddr::from_ptr(syscall_entry as *const ()));
    // `syscall` clears these RFLAGS bits on entry: IF keeps interrupts off until the stub is on a
    // kernel stack, and DF gives kernel code the cleared direction flag the SysV ABI assumes.
    SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::DIRECTION_FLAG);
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

/// Temporary syscall entry target. It prevents accidental fall-through into user memory while
/// the capability-checked dispatcher is being implemented.
extern "C" fn syscall_entry() -> ! {
    super::halt_forever()
}

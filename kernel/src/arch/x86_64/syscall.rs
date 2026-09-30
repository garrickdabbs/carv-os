//! x86-64 syscall entry configuration and user context helpers.
//!
//! The MSR setup is intentionally separate from dispatch: P2.7 will add capability
//! lookup and message decoding without changing the architectural entry contract.

use x86_64::registers::model_specific::Msr;

use super::gdt;

const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;
const RFLAGS_IF: u64 = 1 << 9;
const RFLAGS_DF: u64 = 1 << 10;

/// Saved general-purpose and control state for a preempted thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
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

/// Programs STAR/LSTAR/SFMASK. Call once after the GDT and IDT are loaded.
///
/// The entry stub is a safe placeholder until the syscall dispatcher and IPC ABI land.
pub fn init() {
    let sel = gdt::selectors();
    let star = ((sel.user_code.0 as u64 - 16) << 48) | ((sel.code.0 as u64) << 32);
    // SAFETY: these MSRs are architectural syscall configuration registers and are written once
    // during early boot while interrupts are disabled.
    unsafe {
        Msr::new(IA32_STAR).write(star);
        Msr::new(IA32_LSTAR).write(syscall_entry as usize as u64);
        Msr::new(IA32_FMASK).write(RFLAGS_IF | RFLAGS_DF);
    }
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

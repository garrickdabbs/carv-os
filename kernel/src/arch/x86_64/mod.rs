//! x86_64 primitives: port I/O and interrupt-flag control.

pub mod port;

/// Returns whether interrupts are currently enabled (RFLAGS.IF).
pub fn interrupts_enabled() -> bool {
    let rflags: u64;
    // SAFETY: pushing RFLAGS and popping it into a register has no side effects.
    unsafe {
        core::arch::asm!("pushfq", "pop {}", out(reg) rflags, options(nomem, preserves_flags));
    }
    rflags & (1 << 9) != 0
}

/// Disables interrupts.
pub fn disable_interrupts() {
    // SAFETY: `cli` only clears RFLAGS.IF.
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
}

/// Enables interrupts. Only call when an IDT is installed (P1.1 onwards).
pub fn enable_interrupts() {
    // SAFETY: `sti` only sets RFLAGS.IF; the caller guarantees an IDT exists.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
}

/// Halts the CPU until the next interrupt, forever.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `hlt` only pauses the CPU until the next interrupt; it touches no memory.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

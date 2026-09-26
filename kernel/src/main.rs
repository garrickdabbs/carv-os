//! stock: the Tally OS microkernel.
//!
//! For now this only proves the kernel builds and links in the higher half.
//! Boot (P0.2) and serial output (P0.3) come next.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::panic::PanicInfo;

/// Kernel entry point, named in `linker.ld`.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    halt_forever()
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    halt_forever()
}

fn halt_forever() -> ! {
    loop {
        // SAFETY: `hlt` only pauses the CPU until the next interrupt; it touches no memory.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

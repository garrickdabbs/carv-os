//! Bare-minimum boot console: raw writes to the COM1 data port.
//!
//! This exists only so P0.2 can prove the Limine handoff over QEMU's `-serial stdio`.
//! It does no UART initialisation and never waits for the transmitter, which QEMU
//! tolerates. P0.3 replaces it with a real 16550 driver and `kprintln!`.

use core::fmt;

const COM1_DATA: u16 = 0x3F8;

/// Zero-sized handle implementing [`fmt::Write`] over COM1.
pub struct Uart;

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &byte in s.as_bytes() {
            // SAFETY: writing a byte to the COM1 data port has no memory effects and is
            // harmless even if no UART is present (the write is simply ignored).
            unsafe {
                core::arch::asm!(
                    "out dx, al",
                    in("dx") COM1_DATA,
                    in("al") byte,
                    options(nomem, nostack, preserves_flags)
                );
            }
        }
        Ok(())
    }
}

//! Legacy x86 I/O ports (`in`/`out` instructions).

use core::marker::PhantomData;

/// A single I/O port of a fixed width. Reads and writes are `unsafe` because port I/O can
/// have arbitrary side effects on hardware; the driver owning the port upholds the contract.
#[derive(Clone, Copy, Debug)]
pub struct Port<T> {
    addr: u16,
    _width: PhantomData<T>,
}

impl<T> Port<T> {
    pub const fn new(addr: u16) -> Self {
        Self {
            addr,
            _width: PhantomData,
        }
    }
}

impl Port<u8> {
    /// Reads one byte from the port.
    ///
    /// # Safety
    /// The caller must know that reading this port is safe for the device behind it.
    pub unsafe fn read(self) -> u8 {
        let value: u8;
        // SAFETY: `in` from a port has no memory effects; the caller vouches for the device.
        unsafe {
            core::arch::asm!("in al, dx", out("al") value, in("dx") self.addr, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// Writes one byte to the port.
    ///
    /// # Safety
    /// The caller must know that writing this value to this port is safe for the device behind it.
    pub unsafe fn write(self, value: u8) {
        // SAFETY: `out` to a port has no memory effects; the caller vouches for the device.
        unsafe {
            core::arch::asm!("out dx, al", in("dx") self.addr, in("al") value, options(nomem, nostack, preserves_flags));
        }
    }
}

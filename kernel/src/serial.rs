//! 16550-compatible UART driver for the boot console (COM1), plus `kprint!` / `kprintln!`.
//!
//! Polled output only. QEMU exposes COM1 on `-serial stdio`, which is how every automated
//! test observes the kernel, so this driver has to work before anything else does.

use core::fmt::{self, Write};

use crate::arch::x86_64::port::Port;
use crate::sync::{SpinLock, without_interrupts};

const COM1: u16 = 0x3F8;

/// A 16550 UART at a legacy I/O base address.
pub struct SerialPort {
    data: Port<u8>,        // +0: RBR/THR (DLL when DLAB=1)
    int_enable: Port<u8>,  // +1: IER     (DLM when DLAB=1)
    fifo_ctrl: Port<u8>,   // +2: IIR/FCR
    line_ctrl: Port<u8>,   // +3: LCR
    modem_ctrl: Port<u8>,  // +4: MCR
    line_status: Port<u8>, // +5: LSR
}

/// Line-status bit: transmitter holding register empty.
const LSR_THR_EMPTY: u8 = 1 << 5;

impl SerialPort {
    /// Creates a handle for the UART at `base` without touching hardware.
    ///
    /// # Safety
    /// `base` must be the I/O base of a 16550-compatible UART that nothing else drives.
    pub const unsafe fn new(base: u16) -> Self {
        Self {
            data: Port::new(base),
            int_enable: Port::new(base + 1),
            fifo_ctrl: Port::new(base + 2),
            line_ctrl: Port::new(base + 3),
            modem_ctrl: Port::new(base + 4),
            line_status: Port::new(base + 5),
        }
    }

    /// Programs 115200 8N1 with FIFOs on and interrupts off, then runs the loopback self-test.
    /// Returns `false` if no working UART answered, in which case output is discarded.
    pub fn init(&mut self) -> bool {
        // SAFETY: standard 16550 programming sequence on a port this driver owns.
        unsafe {
            self.int_enable.write(0x00); // no interrupts (polled)
            self.line_ctrl.write(0x80); // DLAB on
            self.data.write(0x01); // divisor = 1 -> 115200 baud
            self.int_enable.write(0x00);
            self.line_ctrl.write(0x03); // 8 data bits, no parity, 1 stop bit, DLAB off
            self.fifo_ctrl.write(0xC7); // enable + clear FIFOs, 14-byte threshold
            self.modem_ctrl.write(0x1E); // loopback mode for the self-test
            self.data.write(0xAE);
            let echoed = self.data.read();
            self.modem_ctrl.write(0x0F); // DTR | RTS | OUT1 | OUT2, loopback off
            echoed == 0xAE
        }
    }

    /// Transmits one byte, waiting for the transmitter to drain first.
    pub fn write_byte(&mut self, byte: u8) {
        // Bounded wait: real hardware always drains; a stuck emulated port must not hang boot.
        for _ in 0..100_000 {
            // SAFETY: reading LSR has no side effects.
            if unsafe { self.line_status.read() } & LSR_THR_EMPTY != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        // SAFETY: THR is writable whenever the port exists; the wait above avoids overrun.
        unsafe { self.data.write(byte) };
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
        Ok(())
    }
}

/// The boot console. `None` until [`init`] runs (or forever, if no UART was found).
// SAFETY: COM1 is the canonical first UART and nothing else in the kernel drives it.
static CONSOLE: SpinLock<Option<SerialPort>> = SpinLock::new(None);

/// Initialises COM1 as the boot console. Returns whether a UART was found.
pub fn init() -> bool {
    // SAFETY: see the `CONSOLE` invariant.
    let mut port = unsafe { SerialPort::new(COM1) };
    let present = port.init();
    if present {
        *CONSOLE.lock() = Some(port);
    }
    present
}

/// Writes one raw byte to the console (the `debug_putc` syscall).
pub fn write_byte(byte: u8) {
    without_interrupts(|| {
        if let Some(port) = CONSOLE.lock().as_mut() {
            port.write_byte(byte);
        }
    });
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    without_interrupts(|| {
        if let Some(port) = CONSOLE.lock().as_mut() {
            let _ = port.write_fmt(args);
        }
    });
}

/// Writes directly to COM1, bypassing the console lock. For the panic path only, where the
/// lock may be held by the code that panicked.
#[doc(hidden)]
pub fn _print_unlocked(args: fmt::Arguments<'_>) {
    // SAFETY: we are about to halt; a torn line is acceptable, a deadlock is not.
    let mut port = unsafe { SerialPort::new(COM1) };
    let _ = port.write_fmt(args);
}

/// Prints to the boot console.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::serial::_print(format_args!($($arg)*)) };
}

/// Prints to the boot console, with a trailing newline.
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => { $crate::serial::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

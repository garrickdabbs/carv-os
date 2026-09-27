//! In-kernel test framework (`cargo xtask test --kernel`).
//!
//! Built only for `cargo test -p chisel --target x86_64-unknown-none`. The test binary is a
//! complete kernel: Limine boots it in QEMU, `kmain` calls the generated `test_main`, every
//! `#[test_case]` function runs on the boot console, and the kernel exits QEMU through the
//! `isa-debug-exit` device with a code that tells `xtask` whether everything passed.

use crate::arch::x86_64::port::Port;
use crate::{kprint, kprintln};

/// QEMU `isa-debug-exit` device (`-device isa-debug-exit,iobase=0xf4,iosize=0x04`). A write of
/// `v` makes QEMU exit with status `(v << 1) | 1`.
const DEBUG_EXIT_PORT: u16 = 0xF4;

/// Exit codes handed to QEMU. `xtask` maps `Success` → 0 and `Failed` → 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum QemuExitCode {
    /// All tests passed. QEMU exits 33.
    Success = 0x10,
    /// A test panicked. QEMU exits 35.
    Failed = 0x11,
}

/// Asks QEMU to exit with `code`. Never returns under QEMU; halts if the device is absent.
pub fn exit_qemu(code: QemuExitCode) -> ! {
    // SAFETY: port 0xF4 is QEMU's isa-debug-exit device, which only terminates the VM; on any
    // other machine the write hits nothing and we simply halt below.
    unsafe { Port::<u8>::new(DEBUG_EXIT_PORT).write(code as u8) };
    crate::arch::x86_64::halt_forever()
}

/// Anything the test runner can execute and name. Implemented for every `fn()`.
pub trait Testable {
    /// Runs the test, printing its name before and `[ok]` after.
    fn run(&self);
}

impl<T: Fn()> Testable for T {
    fn run(&self) {
        kprint!("test {} ... ", core::any::type_name::<T>());
        self();
        kprintln!("[ok]");
    }
}

/// The `#![test_runner]`: runs every `#[test_case]`, then exits QEMU with success. A failing
/// test panics, and the test-build panic handler in `main.rs` exits with `Failed`.
pub fn runner(tests: &[&dyn Testable]) -> ! {
    kprintln!();
    kprintln!("chisel-test: running {} tests", tests.len());
    for test in tests {
        test.run();
    }
    kprintln!("chisel-test: {} passed; 0 failed", tests.len());
    exit_qemu(QemuExitCode::Success)
}

// ---------------------------------------------------------------- the tests

#[test_case]
fn arithmetic_works() {
    let two = core::hint::black_box(2);
    assert_eq!(two + two, 4);
}

#[test_case]
fn console_prints_many_lines() {
    for i in 0..64 {
        kprintln!("console line {i}");
    }
}

#[test_case]
fn console_lock_is_reentrant_across_calls() {
    // Each kprintln! takes and releases the console lock; a second call must not deadlock.
    kprintln!("first");
    kprintln!("second");
}

#[test_case]
fn spinlock_guards_a_value() {
    let lock = crate::sync::SpinLock::new(1u32);
    {
        let mut g = lock.lock();
        *g += 41;
    }
    assert_eq!(*lock.lock(), 42);
}

#[test_case]
fn breakpoint_exception_is_handled_and_resumes() {
    use core::sync::atomic::Ordering;
    let before = crate::arch::x86_64::idt::BREAKPOINTS.load(Ordering::Relaxed);
    x86_64::instructions::interrupts::int3();
    assert_eq!(
        crate::arch::x86_64::idt::BREAKPOINTS.load(Ordering::Relaxed),
        before + 1
    );
}

#[test_case]
fn gdt_selectors_are_live() {
    use x86_64::instructions::segmentation::{CS, Segment};
    let sel = crate::arch::x86_64::gdt::selectors();
    assert_eq!(CS::get_reg(), sel.code);
    assert_ne!(sel.tss.0, 0);
    // The double-fault IST slot points into our own stack, 16-byte aligned.
    let top = crate::arch::x86_64::gdt::double_fault_stack_top().as_u64();
    assert_ne!(top, 0);
    assert_eq!(top % 16, 0);
}

#[test_case]
fn interrupts_are_disabled_at_boot() {
    // Limine hands us the CPU with IF clear and we have no IDT yet; anything else is a bug.
    assert!(!crate::arch::x86_64::interrupts_enabled());
}

//! Interrupt Descriptor Table and CPU exception handlers.
//!
//! Every architectural exception (vectors 0–31 that x86_64 defines) gets a handler, so a fault
//! always produces a readable message on the serial console instead of a silent triple fault.
//! The double-fault handler runs on its own IST stack (see `gdt.rs`) and is therefore reachable
//! even when the faulting code's stack is unusable. Hardware interrupts (vectors 32+) are wired up
//! with the APIC in P1.5.

use core::sync::atomic::{AtomicUsize, Ordering};

use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::sync::StaticCell;
use crate::{kprintln, serial};

static IDT: StaticCell<InterruptDescriptorTable> = StaticCell::new(InterruptDescriptorTable::new());

/// Number of `#BP` exceptions handled since boot (the in-kernel test counts on it).
pub static BREAKPOINTS: AtomicUsize = AtomicUsize::new(0);

/// Fills in and loads the IDT. Call once, after [`super::gdt::init`].
pub fn init() {
    // SAFETY: called once at boot on the only CPU before the table is loaded or read.
    unsafe {
        let idt = IDT.get_mut();
        idt.breakpoint.set_handler_fn(breakpoint);
        idt.double_fault
            .set_handler_fn(double_fault)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        idt.page_fault.set_handler_fn(page_fault);

        idt.divide_error.set_handler_fn(divide_error);
        idt.debug.set_handler_fn(debug);
        idt.non_maskable_interrupt
            .set_handler_fn(non_maskable_interrupt);
        idt.overflow.set_handler_fn(overflow);
        idt.bound_range_exceeded
            .set_handler_fn(bound_range_exceeded);
        idt.invalid_opcode.set_handler_fn(invalid_opcode);
        idt.device_not_available
            .set_handler_fn(device_not_available);
        idt.invalid_tss.set_handler_fn(invalid_tss);
        idt.segment_not_present.set_handler_fn(segment_not_present);
        idt.stack_segment_fault.set_handler_fn(stack_segment_fault);
        idt.general_protection_fault
            .set_handler_fn(general_protection_fault);
        idt.x87_floating_point.set_handler_fn(x87_floating_point);
        idt.alignment_check.set_handler_fn(alignment_check);
        idt.machine_check.set_handler_fn(machine_check);
        idt.simd_floating_point.set_handler_fn(simd_floating_point);
        idt.virtualization.set_handler_fn(virtualization);
        idt.security_exception.set_handler_fn(security_exception);

        IDT.get().load();
    }
}

/// `#BP` — the one exception that resumes: `int3` is how tests prove the IDT works.
extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    BREAKPOINTS.fetch_add(1, Ordering::Relaxed);
    kprintln!(
        "chisel: #BP breakpoint at {:#x} (rsp {:#x})",
        frame.instruction_pointer.as_u64(),
        frame.stack_pointer.as_u64()
    );
}

/// `#DF` — runs on the IST stack. Writes to the console without taking the lock (the faulting
/// code may hold it) and halts: there is nothing to return to.
extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, error_code: u64) -> ! {
    serial::_print_unlocked(format_args!(
        "chisel: DOUBLE FAULT (error code {error_code}) at {:#x}, faulting rsp {:#x}\n{frame:#?}\n",
        frame.instruction_pointer.as_u64(),
        frame.stack_pointer.as_u64()
    ));
    super::halt_forever()
}

/// `#PF` — reports the faulting address and cause, then panics (a kernel page fault is a bug
/// until demand paging exists).
extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let addr = Cr2::read().map(|a| a.as_u64()).unwrap_or(u64::MAX);
    panic!(
        "PAGE FAULT accessing {addr:#x} ({error_code:?}) at {:#x}\n{frame:#?}",
        frame.instruction_pointer.as_u64()
    );
}

/// Exceptions without an error code: report and panic.
macro_rules! fatal {
    ($($name:ident => $label:literal),+ $(,)?) => {$(
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            panic!(
                concat!("EXCEPTION ", $label, " at {:#x}\n{:#?}"),
                frame.instruction_pointer.as_u64(),
                frame
            );
        }
    )+};
}

/// Exceptions that push an error code: report it and panic.
macro_rules! fatal_with_code {
    ($($name:ident => $label:literal),+ $(,)?) => {$(
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, error_code: u64) {
            panic!(
                concat!("EXCEPTION ", $label, " (error code {:#x}) at {:#x}\n{:#?}"),
                error_code,
                frame.instruction_pointer.as_u64(),
                frame
            );
        }
    )+};
}

fatal! {
    divide_error => "#DE divide error",
    debug => "#DB debug",
    non_maskable_interrupt => "NMI",
    overflow => "#OF overflow",
    bound_range_exceeded => "#BR bound range exceeded",
    invalid_opcode => "#UD invalid opcode",
    device_not_available => "#NM device not available",
    x87_floating_point => "#MF x87 floating point",
    simd_floating_point => "#XM SIMD floating point",
    virtualization => "#VE virtualization",
}

fatal_with_code! {
    invalid_tss => "#TS invalid TSS",
    segment_not_present => "#NP segment not present",
    stack_segment_fault => "#SS stack segment fault",
    general_protection_fault => "#GP general protection fault",
    alignment_check => "#AC alignment check",
    security_exception => "#SX security exception",
}

/// `#MC` is diverging: the hardware state is not trustworthy enough to return.
extern "x86-interrupt" fn machine_check(frame: InterruptStackFrame) -> ! {
    panic!(
        "EXCEPTION #MC machine check at {:#x}\n{frame:#?}",
        frame.instruction_pointer.as_u64()
    );
}

//! Interrupt Descriptor Table and CPU exception handlers.
//!
//! Every architectural exception the `x86_64` crate exposes (vectors 0–31, including #CP, #HV
//! and #VC for CET / hypervisor environments) gets a handler, so a fault always produces a
//! readable message on the serial console instead of a silent triple fault.
//! The double-fault handler runs on its own IST stack (see `gdt.rs`) and is therefore reachable
//! even when the faulting code's stack is unusable. Vector 32 is the local APIC timer and 0xFF its
//! spurious vector; the 16 vectors the masked legacy PICs are remapped to get no-op handlers so a
//! spurious IRQ7/IRQ15 is swallowed (P1.5). Vectors 48–71 are the I/O APIC inputs (P2.8), routed
//! to user space through `Irq` objects. An exception raised by ring-3 code is not a kernel bug: it
//! kills the faulting thread (P2.5) and the kernel carries on.

use core::sync::atomic::{AtomicUsize, Ordering};

use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::gdt::DOUBLE_FAULT_IST_INDEX;
use super::pic::PIC_VECTOR_BASE;
use super::{apic, ioapic};
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
        idt.cp_protection_exception
            .set_handler_fn(cp_protection_exception);
        idt.hv_injection_exception
            .set_handler_fn(hv_injection_exception);
        idt.vmm_communication_exception
            .set_handler_fn(vmm_communication_exception);
        idt.security_exception.set_handler_fn(security_exception);

        idt[apic::TIMER_VECTOR].set_handler_fn(apic::timer_interrupt);
        idt[apic::SPURIOUS_VECTOR].set_handler_fn(apic::spurious_interrupt);
        for v in PIC_VECTOR_BASE..=PIC_VECTOR_BASE + 15 {
            idt[v].set_handler_fn(legacy_pic_irq);
        }
        let irqs: [extern "x86-interrupt" fn(InterruptStackFrame); ioapic::MAX_GSI as usize] = [
            device_irq::<0>,
            device_irq::<1>,
            device_irq::<2>,
            device_irq::<3>,
            device_irq::<4>,
            device_irq::<5>,
            device_irq::<6>,
            device_irq::<7>,
            device_irq::<8>,
            device_irq::<9>,
            device_irq::<10>,
            device_irq::<11>,
            device_irq::<12>,
            device_irq::<13>,
            device_irq::<14>,
            device_irq::<15>,
            device_irq::<16>,
            device_irq::<17>,
            device_irq::<18>,
            device_irq::<19>,
            device_irq::<20>,
            device_irq::<21>,
            device_irq::<22>,
            device_irq::<23>,
        ];
        for (gsi, handler) in irqs.into_iter().enumerate() {
            idt[ioapic::IRQ_VECTOR_BASE + gsi as u8].set_handler_fn(handler);
        }

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

/// Whether the interrupted code ran in ring 3.
fn from_user(frame: &InterruptStackFrame) -> bool {
    frame.code_segment.rpl() == x86_64::PrivilegeLevel::Ring3
}

/// `#PF` — a user fault kills the thread; a kernel one reports the faulting address and cause,
/// then panics (a kernel page fault is a bug until demand paging exists).
extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let addr = Cr2::read().map(|a| a.as_u64()).unwrap_or(u64::MAX);
    if from_user(&frame) {
        crate::syscalls::user_fault(
            format_args!("#PF accessing {addr:#x} ({error_code:?})"),
            frame.instruction_pointer.as_u64(),
        );
    }
    panic!(
        "PAGE FAULT accessing {addr:#x} ({error_code:?}) at {:#x}\n{frame:#?}",
        frame.instruction_pointer.as_u64()
    );
}

/// A vector of the masked legacy PICs: only a spurious IRQ7/IRQ15 can get here, and a spurious
/// IRQ must not be acknowledged, so there is nothing to do.
extern "x86-interrupt" fn legacy_pic_irq(_frame: InterruptStackFrame) {}

/// I/O APIC input `GSI`: acknowledge it and signal the notification bound to it, if any.
extern "x86-interrupt" fn device_irq<const GSI: u8>(_frame: InterruptStackFrame) {
    apic::eoi();
    crate::syscalls::irq_fired(GSI);
}

/// Exceptions without an error code: kill a faulting user thread, otherwise report and panic.
macro_rules! fatal {
    ($($name:ident => $label:literal),+ $(,)?) => {$(
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            if from_user(&frame) {
                crate::syscalls::user_fault(
                    format_args!($label),
                    frame.instruction_pointer.as_u64(),
                );
            }
            panic!(
                concat!("EXCEPTION ", $label, " at {:#x}\n{:#?}"),
                frame.instruction_pointer.as_u64(),
                frame
            );
        }
    )+};
}

/// Exceptions that push an error code: kill a faulting user thread, otherwise report it and
/// panic.
macro_rules! fatal_with_code {
    ($($name:ident => $label:literal),+ $(,)?) => {$(
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, error_code: u64) {
            if from_user(&frame) {
                crate::syscalls::user_fault(
                    format_args!(concat!($label, " (error code {:#x})"), error_code),
                    frame.instruction_pointer.as_u64(),
                );
            }
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
    overflow => "#OF overflow",
    bound_range_exceeded => "#BR bound range exceeded",
    invalid_opcode => "#UD invalid opcode",
    device_not_available => "#NM device not available",
    x87_floating_point => "#MF x87 floating point",
    simd_floating_point => "#XM SIMD floating point",
    virtualization => "#VE virtualization",
    hv_injection_exception => "#HV hypervisor injection",
}

fatal_with_code! {
    invalid_tss => "#TS invalid TSS",
    segment_not_present => "#NP segment not present",
    stack_segment_fault => "#SS stack segment fault",
    general_protection_fault => "#GP general protection fault",
    alignment_check => "#AC alignment check",
    cp_protection_exception => "#CP control protection",
    vmm_communication_exception => "#VC VMM communication",
    security_exception => "#SX security exception",
}

/// An NMI is an asynchronous system event, not a fault attributable to user code.
extern "x86-interrupt" fn non_maskable_interrupt(frame: InterruptStackFrame) {
    panic!(
        "EXCEPTION NMI at {:#x}\n{frame:#?}",
        frame.instruction_pointer.as_u64()
    );
}

/// `#MC` is diverging: the hardware state is not trustworthy enough to return.
extern "x86-interrupt" fn machine_check(frame: InterruptStackFrame) -> ! {
    panic!(
        "EXCEPTION #MC machine check at {:#x}\n{frame:#?}",
        frame.instruction_pointer.as_u64()
    );
}

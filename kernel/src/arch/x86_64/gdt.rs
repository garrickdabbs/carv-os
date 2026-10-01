//! Global Descriptor Table and Task State Segment.
//!
//! Long mode barely uses segmentation, but the CPU still needs a GDT with 64-bit code and data
//! descriptors, and a TSS to find the **interrupt stack table** — the separate, known-good stack
//! the CPU switches to for a double fault. Without that switch, a fault caused by a bad stack
//! (overflow into an unmapped guard page, non-canonical `rsp`) would fault again while pushing the
//! exception frame and the machine would triple-fault and reset with no message.
//!
//! The ring-3 segments follow the TSS in the order `syscall`/`sysret` require: `sysretq` loads
//! `SS = STAR[63:48] + 8` and `CS = STAR[63:48] + 16`, so user data must sit 8 bytes *below* user
//! code (see [`super::syscall::init`]).

use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

use crate::sync::StaticCell;

/// IST slot used for the double-fault handler (`idt.rs` references it).
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
/// Size of the double-fault stack. Exception entry plus a few frames of formatting code.
const DOUBLE_FAULT_STACK_SIZE: usize = 5 * 4096;

/// 16-byte aligned so the stack pointer is aligned as the ABI requires on entry.
#[repr(C, align(16))]
struct Stack([u8; DOUBLE_FAULT_STACK_SIZE]);

static DOUBLE_FAULT_STACK: StaticCell<Stack> = StaticCell::new(Stack([0; DOUBLE_FAULT_STACK_SIZE]));
static TSS: StaticCell<TaskStateSegment> = StaticCell::new(TaskStateSegment::new());
static GDT: StaticCell<GlobalDescriptorTable> = StaticCell::new(GlobalDescriptorTable::new());
static SELECTORS: StaticCell<Selectors> = StaticCell::new(Selectors {
    code: SegmentSelector(0),
    data: SegmentSelector(0),
    tss: SegmentSelector(0),
    user_code: SegmentSelector(0),
    user_data: SegmentSelector(0),
});

/// Segment selectors handed out by [`init`].
#[derive(Clone, Copy, Debug)]
pub struct Selectors {
    /// 64-bit kernel code segment (ring 0).
    pub code: SegmentSelector,
    /// Kernel data segment; loaded into SS/DS/ES.
    pub data: SegmentSelector,
    /// The TSS descriptor.
    pub tss: SegmentSelector,
    /// Ring-3 code segment.
    pub user_code: SegmentSelector,
    /// Ring-3 data segment, 8 bytes below `user_code` as `sysretq` requires.
    pub user_data: SegmentSelector,
}

/// Builds and loads the GDT and TSS, then reloads the segment registers. Call once, early in
/// `kmain`, with interrupts disabled and before [`super::idt::init`].
pub fn init() {
    // SAFETY: called once at boot on the only CPU, before anything else touches these statics;
    // afterwards they are only read (by the CPU and by `selectors`).
    unsafe {
        let stack_bottom = VirtAddr::from_ptr(DOUBLE_FAULT_STACK.as_ptr());
        let tss = TSS.get_mut();
        // Stacks grow down: the IST entry is the *top* of the region.
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            stack_bottom + DOUBLE_FAULT_STACK_SIZE as u64;

        let gdt = GDT.get_mut();
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        let tss_sel = gdt.append(Descriptor::tss_segment(TSS.get()));
        // User data first: `sysretq` derives SS as STAR base + 8 and CS as base + 16.
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        *SELECTORS.get_mut() = Selectors {
            code,
            data,
            tss: tss_sel,
            user_code,
            user_data,
        };

        GDT.get().load();
        CS::set_reg(code);
        SS::set_reg(data);
        DS::set_reg(data);
        ES::set_reg(data);
        load_tss(tss_sel);
    }
}

/// Selectors chosen by [`init`]. Zero before it runs.
pub fn selectors() -> Selectors {
    // SAFETY: written once in `init` before any reader; read-only afterwards.
    unsafe { *SELECTORS.get() }
}

/// Top of the double-fault IST stack, for tests and diagnostics.
pub fn double_fault_stack_top() -> VirtAddr {
    // SAFETY: read-only after `init`.
    unsafe { TSS.get().interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] }
}

//! I/O APIC: routes hardware interrupt lines (global system interrupts) to IDT vectors (P2.8).
//!
//! Every input starts masked. An input is unmasked only when an `Irq` object is bound to a
//! notification; its interrupt then arrives on vector [`IRQ_VECTOR_BASE`]` + gsi`, the handler
//! acknowledges it at the local APIC and signals the bound notification (see
//! [`crate::syscalls::irq_fired`]), and a user-space driver waiting on that notification wakes up.
//! Only the first I/O APIC's inputs (GSI 0–23) are handled; that covers QEMU's q35 and i440fx.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::structures::paging::Page;
use x86_64::{PhysAddr, VirtAddr};

use crate::mm::paging;
use crate::platform::acpi::Summary;

/// Vector of GSI 0; GSI `n` uses `IRQ_VECTOR_BASE + n`.
pub const IRQ_VECTOR_BASE: u8 = 48;
/// Inputs handled (the first I/O APIC's redirection entries).
pub const MAX_GSI: u8 = 24;

const WINDOW: u64 = paging::KERNEL_MMIO_BASE + 0x1000;
const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;
const REG_VERSION: u32 = 0x01;
const REDIRECTION_BASE: u32 = 0x10;
const MASKED: u32 = 1 << 16;
const LEVEL: u32 = 1 << 15;
const ACTIVE_LOW: u32 = 1 << 13;

static BASE: AtomicU64 = AtomicU64::new(0);
/// Low redirection-entry bits (polarity, trigger) per GSI, from the MADT's ISA overrides.
static MODES: [AtomicU64; MAX_GSI as usize] = [const { AtomicU64::new(0) }; MAX_GSI as usize];
static INPUTS: AtomicU64 = AtomicU64::new(0);

fn read(reg: u32) -> u32 {
    let base = BASE.load(Ordering::Relaxed);
    // SAFETY: `base` maps the I/O APIC register window (set by `init`); IOREGSEL/IOWIN are the
    // documented 32-bit index/data pair. Callers run with interrupts disabled.
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL) as *mut u32, reg);
        core::ptr::read_volatile((base + IOWIN) as *const u32)
    }
}

fn write(reg: u32, value: u32) {
    let base = BASE.load(Ordering::Relaxed);
    // SAFETY: as in `read`.
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL) as *mut u32, reg);
        core::ptr::write_volatile((base + IOWIN) as *mut u32, value);
    }
}

/// Maps the first I/O APIC from the MADT and masks all of its inputs. Returns how many inputs it
/// has, or `None` without an I/O APIC.
pub fn init(acpi: &Summary) -> Option<u8> {
    let io = acpi.io_apics.iter().find(|io| io.gsi_base == 0)?;
    let page = Page::containing_address(VirtAddr::new(WINDOW));
    // SAFETY: `io.address` is the I/O APIC register page the MADT reports — device memory, never
    // RAM the frame allocator hands out.
    if let Err(e) = unsafe { paging::map_mmio(page, PhysAddr::new(u64::from(io.address))) } {
        panic!("mapping the I/O APIC at {:#x}: {e}", io.address);
    }
    BASE.store(WINDOW + u64::from(io.address) % 4096, Ordering::Relaxed);
    let inputs = (((read(REG_VERSION) >> 16) & 0xff) + 1).min(u32::from(MAX_GSI)) as u8;
    INPUTS.store(u64::from(inputs), Ordering::Relaxed);
    for irq in 0..16 {
        let o = acpi.isa_irq(irq);
        if o.gsi < u32::from(MAX_GSI) {
            let mut mode = 0;
            if o.active_low {
                mode |= ACTIVE_LOW;
            }
            if o.level {
                mode |= LEVEL;
            }
            MODES[o.gsi as usize].store(u64::from(mode), Ordering::Relaxed);
        }
    }
    for gsi in 0..inputs {
        set_masked(gsi, true);
    }
    Some(inputs)
}

/// Whether [`init`] found an I/O APIC with input `gsi`.
pub fn has_input(gsi: u8) -> bool {
    u64::from(gsi) < INPUTS.load(Ordering::Relaxed)
}

/// Masks or unmasks `gsi`, delivering it to the boot CPU on its vector when unmasked.
pub fn set_masked(gsi: u8, masked: bool) {
    if !has_input(gsi) {
        return;
    }
    let mode = MODES[usize::from(gsi)].load(Ordering::Relaxed) as u32;
    let low = u32::from(IRQ_VECTOR_BASE + gsi) | mode | if masked { MASKED } else { 0 };
    let reg = REDIRECTION_BASE + 2 * u32::from(gsi);
    let destination = u32::from(super::apic::id()) << 24;
    write(reg, MASKED);
    write(reg + 1, destination);
    write(reg, low);
}

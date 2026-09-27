//! Local APIC (xAPIC, memory-mapped) and its timer (P1.5).
//!
//! The APIC registers live at the physical base from `IA32_APIC_BASE`, normally `0xFEE0_0000`.
//! That is device memory, not RAM, so Limine's direct map does not cover it (a write there
//! page-faulted on the first attempt); [`init`] maps the page uncached at
//! [`super::super::super::mm::paging::KERNEL_MMIO_BASE`] and reaches it with volatile accesses.
//! The timer runs periodically at 1 kHz on
//! [`TIMER_VECTOR`] after being calibrated against the PIT; every tick bumps [`ticks`]. Hardware
//! IRQ routing to user space (I/O APIC, `Irq` capabilities) comes in P2.8.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::registers::model_specific::Msr;
use x86_64::structures::idt::InterruptStackFrame;

use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::structures::paging::Page;

use super::pit;
use crate::mm::paging;

/// IDT vector the timer fires on (first vector past the CPU exceptions).
pub const TIMER_VECTOR: u8 = 32;
/// Vector the APIC reports spurious interrupts on; its handler does nothing (no EOI either).
pub const SPURIOUS_VECTOR: u8 = 0xFF;
/// Timer frequency the kernel keeps.
pub const TIMER_HZ: u64 = 1000;

const IA32_APIC_BASE: u32 = 0x1B;
const APIC_BASE_ENABLE: u64 = 1 << 11;

// Register offsets (xAPIC).
const REG_ID: u64 = 0x020;
const REG_TPR: u64 = 0x080;
const REG_EOI: u64 = 0x0B0;
const REG_SVR: u64 = 0x0F0;
const REG_LVT_TIMER: u64 = 0x320;
const REG_TIMER_INITIAL: u64 = 0x380;
const REG_TIMER_CURRENT: u64 = 0x390;
const REG_TIMER_DIVIDE: u64 = 0x3E0;

const SVR_ENABLE: u32 = 1 << 8;
const LVT_MASKED: u32 = 1 << 16;
const LVT_PERIODIC: u32 = 1 << 17;
/// Divide-configuration value for "divide by 16".
const DIVIDE_BY_16: u32 = 0b0011;
const CALIBRATION_MS: u64 = 10;

/// Virtual address of the register page once [`init`] has run (0 before).
static BASE: AtomicU64 = AtomicU64::new(0);
/// Timer interrupts handled since [`init`].
static TICKS: AtomicU64 = AtomicU64::new(0);

/// What [`init`] found and programmed; printed at boot.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    /// Physical base of the register page.
    pub phys_base: u64,
    /// This CPU's APIC id.
    pub id: u32,
    /// Timer ticks (after divide-by-16) per millisecond, as measured against the PIT.
    pub ticks_per_ms: u32,
}

fn read(reg: u64) -> u32 {
    let base = BASE.load(Ordering::Relaxed);
    debug_assert!(base != 0, "apic used before init");
    // SAFETY: `base` is the HHDM alias of the APIC register page, set by `init`; registers are
    // 32-bit, 16-byte aligned, and reads have no side effects except on EOI/ISR (not read here).
    unsafe { core::ptr::read_volatile((base + reg) as *const u32) }
}

fn write(reg: u64, value: u32) {
    let base = BASE.load(Ordering::Relaxed);
    debug_assert!(base != 0, "apic used before init");
    // SAFETY: as in `read`; the caller only writes documented register values.
    unsafe { core::ptr::write_volatile((base + reg) as *mut u32, value) }
}

/// Enables the local APIC, calibrates its timer against the PIT and starts it periodic at
/// [`TIMER_HZ`]. Call once, after paging and the IDT are up and the PICs are masked; interrupts
/// may stay disabled — the timer simply queues its first tick.
///
/// # Panics
/// If the register page cannot be mapped, or the calibration measures nothing (no working APIC
/// timer, or the PIT never fired).
pub fn init() -> Info {
    let mut msr = Msr::new(IA32_APIC_BASE);
    // SAFETY: IA32_APIC_BASE exists on every x86_64 CPU; we only set the global-enable bit and
    // keep the base the firmware chose.
    let raw = unsafe {
        let v = msr.read();
        if v & APIC_BASE_ENABLE == 0 {
            msr.write(v | APIC_BASE_ENABLE);
        }
        v | APIC_BASE_ENABLE
    };
    let phys_base = raw & 0x000F_FFFF_FFFF_F000;
    let page = Page::containing_address(VirtAddr::new(paging::KERNEL_MMIO_BASE));
    // SAFETY: `phys_base` is the local APIC register page reported by IA32_APIC_BASE — device
    // memory, never RAM the frame allocator covers.
    if let Err(e) = unsafe { paging::map_mmio(page, PhysAddr::new(phys_base)) } {
        panic!("mapping the local APIC page {phys_base:#x}: {e}");
    }
    BASE.store(page.start_address().as_u64(), Ordering::Relaxed);

    write(REG_SVR, SVR_ENABLE | u32::from(SPURIOUS_VECTOR));
    write(REG_TPR, 0);
    write(REG_LVT_TIMER, LVT_MASKED);
    write(REG_TIMER_DIVIDE, DIVIDE_BY_16);

    // Calibrate: let the timer count down from max while the PIT measures CALIBRATION_MS.
    write(REG_TIMER_INITIAL, u32::MAX);
    pit::busy_wait_ms(CALIBRATION_MS);
    let elapsed = u32::MAX - read(REG_TIMER_CURRENT);
    write(REG_TIMER_INITIAL, 0);
    let ticks_per_ms = elapsed / CALIBRATION_MS as u32;
    assert!(
        ticks_per_ms > 0,
        "APIC timer did not count during calibration"
    );

    // 1 kHz periodic on TIMER_VECTOR.
    TICKS.store(0, Ordering::Relaxed);
    write(REG_LVT_TIMER, LVT_PERIODIC | u32::from(TIMER_VECTOR));
    write(REG_TIMER_INITIAL, ticks_per_ms * (1000 / TIMER_HZ) as u32);

    Info {
        phys_base,
        id: read(REG_ID) >> 24,
        ticks_per_ms,
    }
}

/// This CPU's local APIC id.
pub fn id() -> u8 {
    (read(REG_ID) >> 24) as u8
}

/// Timer ticks since [`init`] (1 kHz once interrupts are enabled).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Signals end-of-interrupt for the interrupt being handled.
fn eoi() {
    write(REG_EOI, 0);
}

/// Vector 32: count the tick and acknowledge it.
pub extern "x86-interrupt" fn timer_interrupt(_frame: InterruptStackFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    eoi();
}

/// Vector 0xFF: a spurious interrupt needs no EOI and no work.
pub extern "x86-interrupt" fn spurious_interrupt(_frame: InterruptStackFrame) {}

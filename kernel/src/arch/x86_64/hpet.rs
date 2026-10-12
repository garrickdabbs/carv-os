//! High Precision Event Timer main counter, used as the LAPIC calibration clock.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::structures::paging::Page;
use x86_64::{PhysAddr, VirtAddr};

use crate::mm::paging;

const WINDOW: u64 = paging::KERNEL_MMIO_BASE + 0x2000;
const REG_CAPABILITIES: u64 = 0x000;
const REG_CONFIGURATION: u64 = 0x010;
const REG_MAIN_COUNTER: u64 = 0x0F0;
const ENABLE: u64 = 1;
const COUNTER_64_BIT: u64 = 1 << 13;
const FEMTOSECONDS_PER_MS: u64 = 1_000_000_000_000;

static BASE: AtomicU64 = AtomicU64::new(0);

/// A mapped, running HPET main counter.
pub struct Hpet {
    period_fs: u64,
    counter_mask: u64,
}

impl Hpet {
    /// Maps and enables the HPET at the physical base reported by ACPI.
    ///
    /// # Panics
    /// Panics if the register block crosses a page, cannot be mapped, or reports an invalid period.
    pub fn init(phys_base: u64) -> Self {
        let offset = phys_base & 0xFFF;
        assert!(
            offset + REG_MAIN_COUNTER + 8 <= 0x1000,
            "HPET register block crosses a page"
        );
        let page = Page::containing_address(VirtAddr::new(WINDOW));
        let phys_page = phys_base & !0xFFF;
        // SAFETY: ACPI identifies `phys_base` as the HPET's device register block, not RAM.
        if let Err(e) = unsafe { paging::map_mmio(page, PhysAddr::new(phys_page)) } {
            panic!("mapping the HPET page {phys_page:#x}: {e}");
        }
        BASE.store(page.start_address().as_u64() + offset, Ordering::Relaxed);

        let capabilities = read(REG_CAPABILITIES);
        let period_fs = capabilities >> 32;
        assert!(period_fs > 0, "HPET reported a zero counter period");
        let counter_mask = if capabilities & COUNTER_64_BIT != 0 {
            u64::MAX
        } else {
            u64::from(u32::MAX)
        };

        write(REG_CONFIGURATION, read(REG_CONFIGURATION) | ENABLE);
        Self {
            period_fs,
            counter_mask,
        }
    }

    /// Busy-waits for `ms` milliseconds using the HPET main counter.
    pub fn busy_wait_ms(&self, ms: u64) {
        let duration_fs = ms
            .checked_mul(FEMTOSECONDS_PER_MS)
            .expect("HPET wait duration overflow");
        let target = duration_fs
            .checked_add(self.period_fs - 1)
            .expect("HPET wait rounding overflow")
            / self.period_fs;
        assert!(
            target <= self.counter_mask,
            "HPET wait exceeds counter range"
        );

        let start = self.counter();
        while self.counter().wrapping_sub(start) & self.counter_mask < target {
            core::hint::spin_loop();
        }
    }

    fn counter(&self) -> u64 {
        read(REG_MAIN_COUNTER) & self.counter_mask
    }
}

fn read(reg: u64) -> u64 {
    let base = BASE.load(Ordering::Relaxed);
    debug_assert!(base != 0, "HPET used before init");
    // SAFETY: `base` is the mapped HPET register block set by `Hpet::init`; these registers are
    // 64-bit aligned and the HPET specification permits 64-bit MMIO reads.
    unsafe { core::ptr::read_volatile((base + reg) as *const u64) }
}

fn write(reg: u64, value: u64) {
    let base = BASE.load(Ordering::Relaxed);
    debug_assert!(base != 0, "HPET used before init");
    // SAFETY: as in `read`; the only write enables the HPET main counter.
    unsafe { core::ptr::write_volatile((base + reg) as *mut u64, value) }
}

//! ACPI tables (P1.6): the RSDP comes from Limine, the tables are parsed with the `acpi` crate,
//! and what later tasks need is kept in a summary: local APIC address and CPU ids, I/O APICs
//! (MADT), the HPET base (HPET), and the PCIe ECAM regions (MCFG, consumed by P1.7).
//!
//! Tables normally sit in ACPI-reclaimable or NVS memory that Limine's direct map covers, so the
//! handler hands out HHDM aliases. A region with any page the direct map does not cover is
//! instead mapped *whole*, page by page and contiguously, into a small window in the kernel
//! dynamic region with [`paging::map_reserved`] (read-only, normal cached memory: these are
//! firmware RAM pages, not device registers). Window pages are never unmapped (the tables are
//! read once and kept as a summary).

use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use acpi::platform::interrupt::InterruptModel;
use acpi::platform::pci::PciConfigRegions;
use acpi::{AcpiTables, Handler, HpetInfo, PciAddress, PhysicalMapping};
use x86_64::structures::paging::Page;
use x86_64::{PhysAddr, VirtAddr};

use crate::mm::paging;
use crate::sync::{SpinLock, without_interrupts};

/// Window for table pages the direct map does not cover: 1 MiB past the MMIO base, bump-allocated.
const TABLE_WINDOW_BASE: u64 = paging::KERNEL_MMIO_BASE + 0x10_0000;
const TABLE_WINDOW_SIZE: u64 = 0x10_0000;
static WINDOW_NEXT: AtomicU64 = AtomicU64::new(TABLE_WINDOW_BASE);

/// One PCIe ECAM region from the MCFG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EcamRegion {
    /// PCI segment group.
    pub segment: u16,
    /// First and last bus number covered.
    pub bus_start: u8,
    /// Last bus number covered.
    pub bus_end: u8,
    /// Physical base of the configuration space for `bus_start`.
    pub base: u64,
}

/// One I/O APIC from the MADT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApic {
    /// I/O APIC id.
    pub id: u8,
    /// Physical register base.
    pub address: u32,
    /// First global system interrupt it handles.
    pub gsi_base: u32,
}

/// An ISA interrupt rerouted by the MADT (e.g. PIT IRQ 0 → GSI 2 on most PCs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IsaOverride {
    /// ISA IRQ number.
    pub isa_irq: u8,
    /// Global system interrupt it is wired to.
    pub gsi: u32,
    /// Active-low input (ISA default: active high).
    pub active_low: bool,
    /// Level-triggered input (ISA default: edge).
    pub level: bool,
}

/// What the tables said; kept for the rest of the kernel's life.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    /// Physical address of the RSDP.
    pub rsdp: u64,
    /// ACPI revision from the RSDP (0 = 1.0, 2 = 2.0+).
    pub revision: u8,
    /// Signatures of every table found in the RSDT/XSDT.
    pub tables: Vec<[u8; 4]>,
    /// Local APIC MMIO base from the MADT.
    pub local_apic_address: u64,
    /// Local APIC ids of every processor (boot processor first).
    pub cpu_apic_ids: Vec<u32>,
    /// I/O APICs from the MADT.
    pub io_apics: Vec<IoApic>,
    /// HPET register base, if an HPET table exists.
    pub hpet_base: Option<u64>,
    /// PCIe ECAM regions from the MCFG.
    pub ecam: Vec<EcamRegion>,
    /// ISA interrupt source overrides from the MADT.
    pub isa_overrides: Vec<IsaOverride>,
}

impl Summary {
    /// The global system interrupt ISA `irq` arrives on, with its polarity and trigger mode
    /// (identity-mapped, active-high, edge-triggered unless the MADT overrides it).
    pub fn isa_irq(&self, irq: u8) -> IsaOverride {
        self.isa_overrides
            .iter()
            .copied()
            .find(|o| o.isa_irq == irq)
            .unwrap_or(IsaOverride {
                isa_irq: irq,
                gsi: u32::from(irq),
                active_low: false,
                level: false,
            })
    }
}

static SUMMARY: SpinLock<Option<Summary>> = SpinLock::new(None);

/// Reaches physical memory through the HHDM, falling back to a fresh mapping for pages the direct
/// map does not cover.
#[derive(Clone)]
struct HhdmHandler {
    hhdm: u64,
}

impl HhdmHandler {
    /// Virtual address of the physical range `[phys, phys + size)` as one contiguous mapping:
    /// the HHDM alias when the direct map covers every page of it, otherwise a fresh window
    /// mapping of the whole range (so a region never straddles two strategies).
    fn virt_for_region(&self, phys: u64, size: u64) -> u64 {
        let first_page = phys & !0xFFF;
        let end = phys + size.max(1);
        let pages = (first_page..end).step_by(0x1000);
        if pages
            .clone()
            .all(|p| paging::translate(VirtAddr::new(self.hhdm + p)).is_some())
        {
            return self.hhdm + phys;
        }
        let count = pages.clone().count() as u64;
        let base = WINDOW_NEXT.fetch_add(count * 0x1000, Ordering::Relaxed);
        assert!(
            base + count * 0x1000 <= TABLE_WINDOW_BASE + TABLE_WINDOW_SIZE,
            "ACPI table window exhausted"
        );
        for (i, page_phys) in pages.enumerate() {
            let page = Page::containing_address(VirtAddr::new(base + i as u64 * 0x1000));
            // SAFETY: the page holds firmware tables the direct map skipped: reserved memory the
            // frame allocator never covers (it only frees usable memmap entries), read here only.
            if let Err(e) = unsafe { paging::map_reserved(page, PhysAddr::new(page_phys)) } {
                panic!("mapping ACPI table page {page_phys:#x}: {e}");
            }
        }
        base + (phys - first_page)
    }
}

impl Handler for HhdmHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let virt = self.virt_for_region(physical_address as u64, size as u64);
        PhysicalMapping {
            physical_start: physical_address,
            virtual_start: NonNull::new(virt as *mut T).expect("ACPI mapping is never null"),
            region_length: size,
            mapped_length: size,
            handler: self.clone(),
        }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {
        // HHDM aliases need no teardown; window pages stay mapped (see the module doc).
    }

    fn read_u8(&self, _address: usize) -> u8 {
        unsupported_acpi_hardware_access()
    }

    fn read_u16(&self, _address: usize) -> u16 {
        unsupported_acpi_hardware_access()
    }

    fn read_u32(&self, _address: usize) -> u32 {
        unsupported_acpi_hardware_access()
    }

    fn read_u64(&self, _address: usize) -> u64 {
        unsupported_acpi_hardware_access()
    }

    fn write_u8(&self, _address: usize, _value: u8) {
        unsupported_acpi_hardware_access()
    }

    fn write_u16(&self, _address: usize, _value: u16) {
        unsupported_acpi_hardware_access()
    }

    fn write_u32(&self, _address: usize, _value: u32) {
        unsupported_acpi_hardware_access()
    }

    fn write_u64(&self, _address: usize, _value: u64) {
        unsupported_acpi_hardware_access()
    }

    fn read_io_u8(&self, _port: u16) -> u8 {
        unsupported_acpi_hardware_access()
    }

    fn read_io_u16(&self, _port: u16) -> u16 {
        unsupported_acpi_hardware_access()
    }

    fn read_io_u32(&self, _port: u16) -> u32 {
        unsupported_acpi_hardware_access()
    }

    fn write_io_u8(&self, _port: u16, _value: u8) {
        unsupported_acpi_hardware_access()
    }

    fn write_io_u16(&self, _port: u16, _value: u16) {
        unsupported_acpi_hardware_access()
    }

    fn write_io_u32(&self, _port: u16, _value: u32) {
        unsupported_acpi_hardware_access()
    }

    fn read_pci_u8(&self, _address: PciAddress, _offset: u16) -> u8 {
        unsupported_acpi_hardware_access()
    }

    fn read_pci_u16(&self, _address: PciAddress, _offset: u16) -> u16 {
        unsupported_acpi_hardware_access()
    }

    fn read_pci_u32(&self, _address: PciAddress, _offset: u16) -> u32 {
        unsupported_acpi_hardware_access()
    }

    fn write_pci_u8(&self, _address: PciAddress, _offset: u16, _value: u8) {
        unsupported_acpi_hardware_access()
    }

    fn write_pci_u16(&self, _address: PciAddress, _offset: u16, _value: u16) {
        unsupported_acpi_hardware_access()
    }

    fn write_pci_u32(&self, _address: PciAddress, _offset: u16, _value: u32) {
        unsupported_acpi_hardware_access()
    }

    fn nanos_since_boot(&self) -> u64 {
        unsupported_acpi_hardware_access()
    }

    fn stall(&self, _microseconds: u64) {
        unsupported_acpi_hardware_access()
    }

    fn sleep(&self, _milliseconds: u64) {
        unsupported_acpi_hardware_access()
    }

    fn create_mutex(&self) -> acpi::Handle {
        unsupported_acpi_hardware_access()
    }

    fn acquire(&self, _mutex: acpi::Handle, _timeout: u16) -> Result<(), acpi::aml::AmlError> {
        unsupported_acpi_hardware_access()
    }

    fn release(&self, _mutex: acpi::Handle) {
        unsupported_acpi_hardware_access()
    }
}

fn unsupported_acpi_hardware_access() -> ! {
    panic!("ACPI hardware access is not implemented")
}

/// Parses the tables reachable from `rsdp` (Limine's RSDP response; an HHDM-virtual address is
/// converted back to physical) and records the [`Summary`]. Call once, after paging and the heap.
///
/// # Panics
/// If the RSDP or the tables are malformed: without ACPI the kernel cannot find its interrupt
/// controllers or PCIe, so there is nothing sensible to boot into.
pub fn init(rsdp: u64, hhdm: u64) -> Summary {
    let rsdp_phys = if rsdp >= hhdm { rsdp - hhdm } else { rsdp };
    let handler = HhdmHandler { hhdm };
    // SAFETY: as above; the handler maps whatever the parser asks for.
    let tables = unsafe { AcpiTables::from_rsdp(handler.clone(), rsdp_phys as usize) }
        .unwrap_or_else(|e| panic!("ACPI: parsing tables from RSDP {rsdp_phys:#x}: {e:?}"));

    let mut summary = Summary {
        rsdp: rsdp_phys,
        revision: tables.rsdp_revision,
        tables: tables
            .table_headers()
            .map(|(_, h)| {
                let mut sig = [0u8; 4];
                sig.copy_from_slice(h.signature.as_str().as_bytes());
                sig
            })
            .collect(),
        ..Summary::default()
    };

    let (interrupt_model, processor_info) =
        InterruptModel::new(&tables).unwrap_or_else(|e| panic!("ACPI: MADT/platform info: {e:?}"));
    if let InterruptModel::Apic(apic) = &interrupt_model {
        summary.local_apic_address = apic.local_apic_address;
        summary.io_apics = apic
            .io_apics
            .iter()
            .map(|io| IoApic {
                id: io.id,
                address: io.address,
                gsi_base: io.global_system_interrupt_base,
            })
            .collect();
        summary.isa_overrides = apic
            .interrupt_source_overrides
            .iter()
            .map(|o| IsaOverride {
                isa_irq: o.isa_source,
                gsi: o.global_system_interrupt,
                active_low: matches!(o.polarity, acpi::platform::interrupt::Polarity::ActiveLow),
                level: matches!(
                    o.trigger_mode,
                    acpi::platform::interrupt::TriggerMode::Level
                ),
            })
            .collect();
    }
    if let Some(cpus) = &processor_info {
        summary.cpu_apic_ids.push(cpus.boot_processor.local_apic_id);
        summary
            .cpu_apic_ids
            .extend(cpus.application_processors.iter().map(|p| p.local_apic_id));
    }
    summary.hpet_base = HpetInfo::new(&tables).ok().map(|h| h.base_address as u64);
    if let Ok(regions) = PciConfigRegions::new(&tables) {
        summary.ecam = regions
            .regions
            .iter()
            .map(|r| EcamRegion {
                segment: r.pci_segment_group,
                bus_start: r.bus_number_start,
                bus_end: r.bus_number_end,
                base: r.base_address,
            })
            .collect();
    }

    without_interrupts(|| *SUMMARY.lock() = Some(summary.clone()));
    summary
}

/// The recorded summary (`None` before [`init`]). Consumers: the in-kernel tests now, PCI(e)
/// enumeration (P1.7) next.
#[cfg_attr(not(test), allow(dead_code))]
pub fn summary() -> Option<Summary> {
    without_interrupts(|| SUMMARY.lock().clone())
}

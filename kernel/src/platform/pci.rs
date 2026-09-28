//! PCI(e) enumeration through the MCFG's ECAM window (P1.7).
//!
//! Every function's 4 KiB configuration page lives at `ecam_base + (bus << 20 | device << 15 |
//! function << 12)`. Probing maps each candidate page into one scratch slot in the kernel's MMIO
//! region, reads the header, and unmaps it again, so enumeration costs no permanent mappings no
//! matter how many buses the MCFG advertises. Bus 0 is walked first and PCI-to-PCI bridges add
//! their secondary buses (only those inside the region's bus range). Drivers (P4) will look devices
//! up in the recorded list and map their own device's page from `config_phys`; interrupt routing
//! waits for the I/O APIC work in P2.8.

use alloc::vec::Vec;

use x86_64::structures::paging::Page;
use x86_64::{PhysAddr, VirtAddr};

use super::acpi::EcamRegion;
use crate::mm::paging;
use crate::sync::{SpinLock, without_interrupts};

/// The one scratch page used to read configuration headers: 4 MiB past the MMIO base.
const CONFIG_SCRATCH: u64 = paging::KERNEL_MMIO_BASE + 0x40_0000;

/// Red Hat / Qumranet: every virtio device.
pub const VENDOR_VIRTIO: u16 = 0x1AF4;

/// One discovered PCI function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciDevice {
    /// PCI segment group.
    pub segment: u16,
    /// Bus number.
    pub bus: u8,
    /// Device number (0–31).
    pub device: u8,
    /// Function number (0–7).
    pub function: u8,
    /// Vendor id.
    pub vendor: u16,
    /// Device id.
    pub device_id: u16,
    /// Class code (byte 0x0B).
    pub class: u8,
    /// Subclass (byte 0x0A).
    pub subclass: u8,
    /// Programming interface (byte 0x09).
    pub prog_if: u8,
    /// Header type without the multi-function bit.
    pub header_type: u8,
    /// Physical address of this function's configuration page.
    pub config_phys: u64,
}

impl PciDevice {
    /// A human name for the virtio devices the plan cares about, else `None`.
    pub fn virtio_name(&self) -> Option<&'static str> {
        if self.vendor != VENDOR_VIRTIO {
            return None;
        }
        Some(match self.device_id {
            0x1000 | 0x1041 => "virtio-net",
            0x1001 | 0x1042 => "virtio-blk",
            0x1005 | 0x1044 => "virtio-rng",
            0x1003 | 0x1043 => "virtio-console",
            0x1009 | 0x1049 => "virtio-9p",
            _ => "virtio (other)",
        })
    }
}

static DEVICES: SpinLock<Vec<PciDevice>> = SpinLock::new(Vec::new());

/// Physical address of the configuration page for `(bus, device, function)` in `region`;
/// `bus` must lie inside the region's bus range.
fn config_phys(region: &EcamRegion, bus: u8, device: u8, function: u8) -> u64 {
    debug_assert!((region.bus_start..=region.bus_end).contains(&bus));
    region.base
        + ((u64::from(bus - region.bus_start) << 20)
            | (u64::from(device) << 15)
            | (u64::from(function) << 12))
}

/// Maps the configuration page at `phys` into the scratch slot, runs `f` on it, unmaps it.
fn with_config_page<R>(phys: u64, f: impl FnOnce(u64) -> R) -> R {
    let page = Page::containing_address(VirtAddr::new(CONFIG_SCRATCH));
    // SAFETY: `phys` is inside the ECAM range the firmware reserved for PCIe configuration
    // space (device memory, never RAM the frame allocator covers); the scratch page is unmapped
    // again below, so nothing else can observe it.
    if let Err(e) = unsafe { paging::map_mmio(page, PhysAddr::new(phys)) } {
        panic!("mapping PCI config page {phys:#x}: {e}");
    }
    let result = f(CONFIG_SCRATCH);
    // The returned `Frame` is the device page, not allocator memory: it is dropped, never freed.
    if let Err(e) = paging::unmap(page) {
        panic!("unmapping PCI config scratch page: {e}");
    }
    result
}

fn read_u32(config: u64, offset: u64) -> u32 {
    // SAFETY: `config` is a mapped configuration page and `offset` stays inside its 4 KiB;
    // reading configuration registers has no side effects for the header fields used here.
    unsafe { core::ptr::read_volatile((config + offset) as *const u32) }
}

/// Header facts read in one visit to the scratch page.
struct Probe {
    dev: PciDevice,
    multi_function: bool,
    /// Secondary bus number when the function is a PCI-to-PCI bridge.
    secondary_bus: Option<u8>,
}

/// Reads the header of one function, `None` if no device answers (vendor `0xFFFF`).
fn probe(region: &EcamRegion, bus: u8, device: u8, function: u8) -> Option<Probe> {
    let phys = config_phys(region, bus, device, function);
    with_config_page(phys, |config| {
        let id = read_u32(config, 0x00);
        let vendor = (id & 0xFFFF) as u16;
        if vendor == 0xFFFF {
            return None;
        }
        let class_reg = read_u32(config, 0x08);
        let header = read_u32(config, 0x0C);
        let header_type = ((header >> 16) & 0x7F) as u8;
        let secondary_bus =
            (header_type == 1).then(|| ((read_u32(config, 0x18) >> 8) & 0xFF) as u8);
        Some(Probe {
            dev: PciDevice {
                segment: region.segment,
                bus,
                device,
                function,
                vendor,
                device_id: (id >> 16) as u16,
                class: (class_reg >> 24) as u8,
                subclass: (class_reg >> 16) as u8,
                prog_if: (class_reg >> 8) as u8,
                header_type,
                config_phys: phys,
            },
            multi_function: (header >> 16) & 0x80 != 0,
            secondary_bus,
        })
    })
}

/// Walks every ECAM region: bus 0 of each, plus the secondary buses of the bridges found.
/// Records and returns the devices. Call once, after ACPI.
pub fn enumerate(regions: &[EcamRegion]) -> Vec<PciDevice> {
    let mut found = Vec::new();
    for region in regions {
        let mut buses: Vec<u8> = alloc::vec![region.bus_start];
        let mut i = 0;
        while i < buses.len() {
            let bus = buses[i];
            i += 1;
            for device in 0..32u8 {
                let Some(first) = probe(region, bus, device, 0) else {
                    continue;
                };
                let functions = if first.multi_function { 8 } else { 1 };
                let mut first = Some(first);
                for function in 0..functions {
                    let entry = match first.take() {
                        Some(p) => Some(p),
                        None => probe(region, bus, device, function),
                    };
                    let Some(p) = entry else { continue };
                    // A bridge's secondary bus is walked only if this region can address it.
                    if let Some(secondary) = p.secondary_bus
                        && secondary != 0
                        && (region.bus_start..=region.bus_end).contains(&secondary)
                        && !buses.contains(&secondary)
                    {
                        buses.push(secondary);
                    }
                    found.push(p.dev);
                }
            }
        }
    }
    without_interrupts(|| *DEVICES.lock() = found.clone());
    found
}

/// Every function found by [`enumerate`] (empty before it runs). Consumers: the in-kernel tests
/// now, the virtio drivers (P4) next.
#[cfg_attr(not(test), allow(dead_code))]
pub fn devices() -> Vec<PciDevice> {
    without_interrupts(|| DEVICES.lock().clone())
}

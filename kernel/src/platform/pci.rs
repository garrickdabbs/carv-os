//! PCI(e) enumeration through the MCFG's ECAM window (P1.7).
//!
//! Every function's 4 KiB configuration page lives at `ecam_base + (bus << 20 | device << 15 |
//! function << 12)`. Pages are mapped on demand into the kernel's MMIO region (one page per
//! probed function, bump-allocated, never unmapped: the enumeration runs once). Bus 0 is walked
//! first and PCI-to-PCI bridges add their secondary buses. Drivers (P4) will look devices up in
//! the recorded list; interrupt routing waits for the I/O APIC work in P2.8.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::structures::paging::Page;
use x86_64::{PhysAddr, VirtAddr};

use super::acpi::EcamRegion;
use crate::mm::paging;
use crate::sync::{SpinLock, without_interrupts};

/// Window for configuration pages: 4 MiB past the MMIO base, 8 MiB (2048 functions) of room.
const CONFIG_WINDOW_BASE: u64 = paging::KERNEL_MMIO_BASE + 0x40_0000;
const CONFIG_WINDOW_SIZE: u64 = 0x80_0000;
static WINDOW_NEXT: AtomicU64 = AtomicU64::new(CONFIG_WINDOW_BASE);

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

/// Maps the configuration page for `(bus, device, function)` in `region` and returns its
/// virtual address.
fn map_config_page(region: &EcamRegion, bus: u8, device: u8, function: u8) -> u64 {
    let phys = region.base
        + ((u64::from(bus - region.bus_start) << 20)
            | (u64::from(device) << 15)
            | (u64::from(function) << 12));
    let virt = WINDOW_NEXT.fetch_add(0x1000, Ordering::Relaxed);
    assert!(
        virt < CONFIG_WINDOW_BASE + CONFIG_WINDOW_SIZE,
        "PCI configuration window exhausted"
    );
    let page = Page::containing_address(VirtAddr::new(virt));
    // SAFETY: `phys` is inside the ECAM range the firmware reserved for PCIe configuration
    // space (device memory, never RAM the frame allocator covers).
    if let Err(e) = unsafe { paging::map_mmio(page, PhysAddr::new(phys)) } {
        panic!("mapping PCI config page {phys:#x}: {e}");
    }
    virt
}

fn read_u32(config: u64, offset: u64) -> u32 {
    // SAFETY: `config` is a mapped configuration page and `offset` stays inside its 4 KiB;
    // reading configuration registers has no side effects for the header fields used here.
    unsafe { core::ptr::read_volatile((config + offset) as *const u32) }
}

/// Reads the header of one function, `None` if no device answers (vendor `0xFFFF`).
fn probe(region: &EcamRegion, bus: u8, device: u8, function: u8) -> Option<(PciDevice, u64)> {
    let config = map_config_page(region, bus, device, function);
    let id = read_u32(config, 0x00);
    let vendor = (id & 0xFFFF) as u16;
    if vendor == 0xFFFF {
        return None;
    }
    let class_reg = read_u32(config, 0x08);
    let header = read_u32(config, 0x0C);
    let dev = PciDevice {
        segment: region.segment,
        bus,
        device,
        function,
        vendor,
        device_id: (id >> 16) as u16,
        class: (class_reg >> 24) as u8,
        subclass: (class_reg >> 16) as u8,
        prog_if: (class_reg >> 8) as u8,
        header_type: ((header >> 16) & 0x7F) as u8,
        config_phys: region.base
            + ((u64::from(bus - region.bus_start) << 20)
                | (u64::from(device) << 15)
                | (u64::from(function) << 12)),
    };
    Some((dev, config))
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
                let Some((dev0, cfg0)) = probe(region, bus, device, 0) else {
                    continue;
                };
                let multi = (read_u32(cfg0, 0x0C) >> 16) & 0x80 != 0;
                let functions = if multi { 8 } else { 1 };
                for function in 0..functions {
                    let entry = if function == 0 {
                        Some((dev0, cfg0))
                    } else {
                        probe(region, bus, device, function)
                    };
                    let Some((dev, cfg)) = entry else { continue };
                    // A PCI-to-PCI bridge (header type 1) exposes its secondary bus at 0x19.
                    if dev.header_type == 1 {
                        let secondary = ((read_u32(cfg, 0x18) >> 8) & 0xFF) as u8;
                        if secondary != 0
                            && secondary <= region.bus_end
                            && !buses.contains(&secondary)
                        {
                            buses.push(secondary);
                        }
                    }
                    found.push(dev);
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

//! Kernel page-table mapper.
//!
//! Limine leaves us in long mode with its own page tables: the kernel at `0xffffffff80000000`
//! and every physical frame reachable through the higher-half direct map (HHDM). We keep using
//! those tables — page-table frames are ordinary physical frames, so the HHDM lets us read and
//! write them directly — and add mappings under our own region, [`KERNEL_DYNAMIC_BASE`], for
//! test pages now and the kernel heap (P1.4) next. New page-table frames come from
//! [`super::frame`]. Address spaces for user processes (P2.4) will each get their own root table.
//!
//! `map` and `unmap` only accept pages inside the dynamic region: the HHDM and the kernel image
//! are what the kernel is executing through, and nothing should be able to remap them by
//! accident through a safe API. `map` also refuses frames the allocator has not handed out, so
//! frame 0, the bitmap or firmware memory cannot be mapped by mistake. Lock acquisitions run
//! with interrupts disabled.

use x86_64::registers::control::Cr3;
use x86_64::structures::paging::mapper::{MapToError, UnmapError};
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags,
    PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use super::frame::{self, Frame};
use crate::sync::{SpinLock, without_interrupts};

/// Start of the kernel's dynamically mapped region: PML4 slot 0x120, well above the HHDM (slot
/// 0x100 onwards covers physical memory) and below the kernel image (slot 0x1ff).
pub const KERNEL_DYNAMIC_BASE: u64 = 0xffff_9000_0000_0000;
/// Size of the dynamic region: one PML4 slot, 512 GiB.
pub const KERNEL_DYNAMIC_SIZE: u64 = 1 << 39;

/// Flags for ordinary kernel data pages: present, writable, not executable (W^X).
pub const KERNEL_DATA: PageTableFlags = PageTableFlags::PRESENT
    .union(PageTableFlags::WRITABLE)
    .union(PageTableFlags::NO_EXECUTE);

/// Why [`map`] refused. The frame the caller passed in is handed back alongside it.
#[derive(Debug)]
pub enum MapError {
    /// The page is not inside the kernel dynamic region.
    OutsideDynamicRegion,
    /// The frame is not currently allocated by `mm::frame` (reserved, freed, or never handed out),
    /// so the caller cannot own it.
    FrameNotOwned,
    /// The page-table walk failed (already mapped, or no frame for an intermediate table).
    Mapper(MapToError<Size4KiB>),
}

impl core::fmt::Display for MapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MapError::OutsideDynamicRegion => {
                f.write_str("page is outside the kernel dynamic region")
            }
            MapError::FrameNotOwned => {
                f.write_str("frame is not an allocated frame (reserved or freed)")
            }
            MapError::Mapper(e) => write!(f, "page-table walk failed: {e:?}"),
        }
    }
}

/// Why [`unmap`] refused.
#[derive(Debug)]
pub enum UnmapFail {
    /// The page is not inside the kernel dynamic region.
    OutsideDynamicRegion,
    /// The page-table walk failed (not mapped, or a huge page).
    Mapper(UnmapError),
}

impl core::fmt::Display for UnmapFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            UnmapFail::OutsideDynamicRegion => {
                f.write_str("page is outside the kernel dynamic region")
            }
            UnmapFail::Mapper(e) => write!(f, "page-table walk failed: {e:?}"),
        }
    }
}

static MAPPER: SpinLock<Option<OffsetPageTable<'static>>> = SpinLock::new(None);

/// Adapts the frame allocator for page-table frames.
struct FrameSource;

// SAFETY: `frame::allocate` only ever returns frames that are usable RAM and not handed out to
// anyone else, which is exactly the contract `FrameAllocator` requires.
unsafe impl FrameAllocator<Size4KiB> for FrameSource {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        frame::allocate().map(|f| PhysFrame::containing_address(f.start()))
    }
}

impl FrameDeallocator<Size4KiB> for FrameSource {
    unsafe fn deallocate_frame(&mut self, f: PhysFrame) {
        frame::free(Frame::new(f.start_address()).expect("PhysFrame is always aligned"));
    }
}

/// Runs `f` with the mapper locked and interrupts disabled.
fn with_mapper<R>(f: impl FnOnce(&mut OffsetPageTable<'static>) -> R) -> R {
    without_interrupts(|| {
        let mut guard = MAPPER.lock();
        f(guard.as_mut().expect("paging not initialised"))
    })
}

/// Whether `page` lies inside the kernel dynamic region.
pub fn in_dynamic_region(page: Page<Size4KiB>) -> bool {
    let a = page.start_address().as_u64();
    (KERNEL_DYNAMIC_BASE..KERNEL_DYNAMIC_BASE + KERNEL_DYNAMIC_SIZE).contains(&a)
}

/// Adopts the bootloader's page tables. Call once, after [`super::frame::init`].
///
/// # Safety
/// `hhdm_offset` must be Limine's HHDM offset (every physical address `p` is mapped at
/// `hhdm_offset + p`), and the active CR3 must point at tables that stay mapped and writable
/// through the HHDM for the rest of the kernel's life.
pub unsafe fn init(hhdm_offset: u64) {
    let (l4_frame, _) = Cr3::read();
    let l4_virt = hhdm_offset + l4_frame.start_address().as_u64();
    // SAFETY: the caller guarantees the HHDM mapping, so this points at the live level-4 table;
    // we take the only reference to it for the kernel's lifetime.
    let l4: &'static mut PageTable = unsafe { &mut *(l4_virt as *mut PageTable) };
    // SAFETY: `l4` is the active level-4 table and `hhdm_offset` is the physical-memory offset
    // the mapper needs to reach every lower-level table.
    let mapper = unsafe { OffsetPageTable::new(l4, VirtAddr::new(hhdm_offset)) };
    without_interrupts(|| *MAPPER.lock() = Some(mapper));
}

/// Maps `page` (inside the dynamic region) to `frame` with `flags`, allocating intermediate
/// tables as needed, and flushes the TLB entry. On failure the frame is returned to the caller,
/// who still owns it.
pub fn map(
    page: Page<Size4KiB>,
    frame: Frame,
    flags: PageTableFlags,
) -> Result<(), (MapError, Frame)> {
    if !in_dynamic_region(page) {
        return Err((MapError::OutsideDynamicRegion, frame));
    }
    if !frame::is_allocated(frame) {
        return Err((MapError::FrameNotOwned, frame));
    }
    let result = with_mapper(|mapper| {
        // SAFETY: the caller owns `frame` (it came from `frame::allocate`) and `page` is inside the
        // kernel's dynamic region, so this mapping cannot alias memory anyone else relies on.
        unsafe {
            mapper.map_to(
                page,
                PhysFrame::containing_address(frame.start()),
                flags,
                &mut FrameSource,
            )
        }
        .map(|flush| flush.flush())
    });
    result.map_err(|e| (MapError::Mapper(e), frame))
}

/// Removes the mapping for `page` (inside the dynamic region), flushes the TLB entry, and returns
/// the frame it pointed at (the caller decides whether to free it).
pub fn unmap(page: Page<Size4KiB>) -> Result<Frame, UnmapFail> {
    if !in_dynamic_region(page) {
        return Err(UnmapFail::OutsideDynamicRegion);
    }
    let phys = with_mapper(|mapper| {
        mapper.unmap(page).map(|(phys, flush)| {
            flush.flush();
            phys
        })
    })
    .map_err(UnmapFail::Mapper)?;
    Ok(Frame::new(phys.start_address()).expect("PhysFrame is always aligned"))
}

/// Physical address `virt` currently maps to, if any. Read-only, so any address is allowed.
pub fn translate(virt: VirtAddr) -> Option<PhysAddr> {
    with_mapper(|mapper| mapper.translate_addr(virt))
}

/// Maps `pages` fresh frames back-to-back starting at `start` (inside the dynamic region),
/// leaving the page *below* `start` unmapped as a guard. Used for the stack-overflow test now and
/// kernel stacks later.
///
/// # Panics
/// If frames run out or a page cannot be mapped — both are fatal during kernel setup.
pub fn map_stack_with_guard(start: VirtAddr, pages: usize) -> VirtAddr {
    for i in 0..pages {
        let page = Page::containing_address(start + (i * 4096) as u64);
        let f = frame::allocate().expect("out of frames");
        if let Err((e, f)) = map(page, f, KERNEL_DATA) {
            frame::free(f);
            panic!(
                "mapping stack page {:#x}: {e}",
                page.start_address().as_u64()
            );
        }
    }
    start + (pages * 4096) as u64
}

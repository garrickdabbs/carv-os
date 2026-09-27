//! Kernel page-table mapper.
//!
//! Limine leaves us in long mode with its own page tables: the kernel at `0xffffffff80000000`
//! and every physical frame reachable through the higher-half direct map (HHDM). We keep using
//! those tables — page-table frames are ordinary physical frames, so the HHDM lets us read and
//! write them directly — and add mappings under our own region, [`KERNEL_DYNAMIC_BASE`], for
//! test pages now and the kernel heap (P1.4) next. New page-table frames come from
//! [`super::frame`]. Address spaces for user processes (P2.4) will each get their own root table.

use x86_64::registers::control::Cr3;
use x86_64::structures::paging::mapper::{MapToError, UnmapError};
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags,
    PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use super::frame::{self, Frame};
use crate::sync::SpinLock;

/// Start of the kernel's dynamically mapped region: PML4 slot 0x120, well above the HHDM (slot
/// 0x100 onwards covers physical memory) and below the kernel image (slot 0x1ff).
pub const KERNEL_DYNAMIC_BASE: u64 = 0xffff_9000_0000_0000;

/// Flags for ordinary kernel data pages: present, writable, not executable (W^X).
pub const KERNEL_DATA: PageTableFlags = PageTableFlags::PRESENT
    .union(PageTableFlags::WRITABLE)
    .union(PageTableFlags::NO_EXECUTE);

static MAPPER: SpinLock<Option<OffsetPageTable<'static>>> = SpinLock::new(None);

/// Adapts the frame allocator for page-table frames.
struct FrameSource;

// SAFETY: `frame::allocate` only ever returns frames that are usable RAM and not handed out to
// anyone else, which is exactly the contract `FrameAllocator` requires.
unsafe impl FrameAllocator<Size4KiB> for FrameSource {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        frame::allocate().map(|f| PhysFrame::containing_address(f.0))
    }
}

impl FrameDeallocator<Size4KiB> for FrameSource {
    unsafe fn deallocate_frame(&mut self, f: PhysFrame) {
        frame::free(Frame(f.start_address()));
    }
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
    *MAPPER.lock() = Some(mapper);
}

/// Maps `page` to `frame` with `flags`, allocating intermediate tables as needed, and flushes
/// the TLB entry.
pub fn map(
    page: Page<Size4KiB>,
    frame: Frame,
    flags: PageTableFlags,
) -> Result<(), MapToError<Size4KiB>> {
    let mut guard = MAPPER.lock();
    let mapper = guard.as_mut().expect("paging not initialised");
    // SAFETY: the caller owns `frame` (it came from `frame::allocate`) and `page` lies in the
    // kernel's dynamic region, so creating this mapping cannot alias memory anyone else relies on.
    let flush = unsafe {
        mapper.map_to(
            page,
            PhysFrame::containing_address(frame.0),
            flags,
            &mut FrameSource,
        )?
    };
    flush.flush();
    Ok(())
}

/// Removes the mapping for `page`, flushes the TLB entry, and returns the frame it pointed at
/// (the caller decides whether to free it).
pub fn unmap(page: Page<Size4KiB>) -> Result<Frame, UnmapError> {
    let mut guard = MAPPER.lock();
    let mapper = guard.as_mut().expect("paging not initialised");
    let (phys, flush) = mapper.unmap(page)?;
    flush.flush();
    Ok(Frame(phys.start_address()))
}

/// Physical address `virt` currently maps to, if any.
pub fn translate(virt: VirtAddr) -> Option<PhysAddr> {
    MAPPER
        .lock()
        .as_ref()
        .expect("paging not initialised")
        .translate_addr(virt)
}

/// Maps `pages` fresh frames back-to-back starting at `start`, leaving the page *below* `start`
/// unmapped as a guard. Used for the stack-overflow test now and kernel stacks later.
pub fn map_stack_with_guard(start: VirtAddr, pages: usize) -> VirtAddr {
    for i in 0..pages {
        let page = Page::containing_address(start + (i * 4096) as u64);
        let f = frame::allocate().expect("out of frames");
        map(page, f, KERNEL_DATA).expect("mapping stack page");
    }
    start + (pages * 4096) as u64
}

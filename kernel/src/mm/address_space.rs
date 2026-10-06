//! User address spaces (P2.5, P2.9).
//!
//! Each address space has its own level-4 table. The upper half (entries 256–511: the HHDM, the
//! kernel dynamic region and the kernel image) is copied from the kernel's table, so kernel code
//! keeps running across a CR3 switch; those entries are supervisor-only. The lower half belongs to
//! the user: pages are only ever mapped below [`carv_abi::USER_TOP`], each backed by a fresh
//! zeroed frame the address space owns and frees (together with its page tables) when dropped.
//!
//! The kernel never dereferences user virtual addresses. [`AddressSpace::read`] and
//! [`AddressSpace::write`] walk the page tables in software and copy through the HHDM, so a bad
//! user pointer is an error instead of a kernel page fault, and SMAP stays on.

use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use super::frame::{self, Frame};
use super::paging::{self, FrameSource};

const PAGE: u64 = 4096;

/// Access a user copy needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The kernel loading an image: any present user page, writable or not.
    Kernel,
    /// On behalf of user code reading: the page must be user-accessible.
    UserRead,
    /// On behalf of user code writing: the page must be user-accessible and writable.
    UserWrite,
}

/// Why a mapping was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapUserError {
    /// The address is not page aligned or not below [`carv_abi::USER_TOP`].
    BadAddress,
    /// Writable and executable at once (W^X).
    WritableExecutable,
    /// The page is already mapped.
    AlreadyMapped,
    /// No frame for the page or a page table.
    OutOfMemory,
}

/// A user address space: its own level-4 table plus the frames mapped under it.
pub struct AddressSpace {
    root: PhysFrame,
    pages: usize,
}

fn hhdm<T>(phys: PhysAddr) -> *mut T {
    (paging::hhdm_offset() + phys.as_u64()) as *mut T
}

fn zero_frame(f: Frame) {
    // SAFETY: the frame was just allocated, so nothing else references it, and the HHDM maps all
    // usable RAM writable.
    unsafe { core::ptr::write_bytes(hhdm::<u8>(f.start()), 0, PAGE as usize) };
}

impl AddressSpace {
    /// Creates an empty address space sharing the kernel's upper half. `None` if out of frames.
    pub fn new() -> Option<Self> {
        let f = frame::allocate()?;
        zero_frame(f);
        let kernel: *const PageTable = hhdm(paging::kernel_root().start_address());
        let table: *mut PageTable = hhdm(f.start());
        // SAFETY: both tables are live level-4 tables reached through the HHDM; the new one is
        // ours alone. Copying the upper-half entries shares the kernel's lower-level tables.
        let (table, kernel) = unsafe { (&mut *table, &*kernel) };
        for i in 256..512 {
            table[i] = kernel[i].clone();
        }
        Some(Self {
            root: PhysFrame::containing_address(f.start()),
            pages: 0,
        })
    }

    /// The level-4 table to load into CR3.
    pub fn root(&self) -> PhysFrame {
        self.root
    }

    /// Number of user pages mapped.
    pub fn mapped_pages(&self) -> usize {
        self.pages
    }

    /// Maps a fresh zeroed frame at `va` (page aligned, below `USER_TOP`), user-accessible,
    /// writable and/or executable as asked (never both).
    pub fn map_zeroed(&mut self, va: u64, write: bool, exec: bool) -> Result<(), MapUserError> {
        if va % PAGE != 0 || va >= carv_abi::USER_TOP {
            return Err(MapUserError::BadAddress);
        }
        if write && exec {
            return Err(MapUserError::WritableExecutable);
        }
        if self.translate(va).is_some() {
            return Err(MapUserError::AlreadyMapped);
        }
        let f = frame::allocate().ok_or(MapUserError::OutOfMemory)?;
        zero_frame(f);
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if write {
            flags |= PageTableFlags::WRITABLE;
        }
        if !exec {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        let parents =
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        let table: *mut PageTable = hhdm(self.root.start_address());
        // SAFETY: `table` is this address space's level-4 table, only reached through `&mut self`;
        // the HHDM offset lets the mapper reach every lower-level table.
        let mut mapper =
            unsafe { OffsetPageTable::new(&mut *table, VirtAddr::new(paging::hhdm_offset())) };
        let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(va));
        // SAFETY: the frame is freshly allocated and owned by this address space; the page is in
        // the user half, which no kernel code relies on.
        let result = unsafe {
            mapper.map_to_with_table_flags(
                page,
                PhysFrame::containing_address(f.start()),
                flags,
                parents,
                &mut FrameSource,
            )
        };
        match result {
            Ok(flush) => {
                // Harmless if this address space is not active; required if it is.
                flush.flush();
                self.pages += 1;
                Ok(())
            }
            Err(_) => {
                frame::free(f);
                Err(MapUserError::OutOfMemory)
            }
        }
    }

    /// Walks the tables for `va` in the user half: the physical address and the effective
    /// (user, writable) permission, or `None` when unmapped.
    pub fn translate(&self, va: u64) -> Option<(PhysAddr, bool, bool)> {
        if va >= carv_abi::USER_TOP {
            return None;
        }
        let indices = [
            (va >> 39) & 0x1ff,
            (va >> 30) & 0x1ff,
            (va >> 21) & 0x1ff,
            (va >> 12) & 0x1ff,
        ];
        let mut table = self.root.start_address();
        let mut user = true;
        let mut writable = true;
        for (level, index) in indices.iter().enumerate() {
            // SAFETY: `table` is a page table of this address space (the root or one reached
            // through a present entry), mapped through the HHDM.
            let entry = unsafe { &(&*hhdm::<PageTable>(table))[*index as usize] };
            let flags = entry.flags();
            if !flags.contains(PageTableFlags::PRESENT)
                || (level < 3 && flags.contains(PageTableFlags::HUGE_PAGE))
            {
                return None;
            }
            user &= flags.contains(PageTableFlags::USER_ACCESSIBLE);
            writable &= flags.contains(PageTableFlags::WRITABLE);
            table = entry.addr();
        }
        Some((table + (va & (PAGE - 1)), user, writable))
    }

    fn check(&self, va: u64, access: Access) -> Option<PhysAddr> {
        let (phys, user, writable) = self.translate(va)?;
        let ok = match access {
            Access::Kernel => true,
            Access::UserRead => user,
            Access::UserWrite => user && writable,
        };
        ok.then_some(phys)
    }

    fn validate(&self, va: u64, len: usize, access: Access) -> bool {
        let Some(end) = va.checked_add(len as u64) else {
            return false;
        };
        if end > carv_abi::USER_TOP {
            return false;
        }
        let mut page = va & !(PAGE - 1);
        while page < end {
            if self.check(page, access).is_none() {
                return false;
            }
            page += PAGE;
        }
        true
    }

    /// Whether `[va, va + len)` is fully mapped with `access`.
    pub fn accessible(&self, va: u64, len: usize, access: Access) -> bool {
        self.validate(va, len, access)
    }

    /// Copies `bytes` to user address `va`. Nothing is written unless the whole range passes the
    /// `access` check.
    pub fn write(&mut self, va: u64, bytes: &[u8], access: Access) -> bool {
        if !self.validate(va, bytes.len(), access) {
            return false;
        }
        let mut done = 0;
        while done < bytes.len() {
            let at = va + done as u64;
            let chunk = ((PAGE - (at & (PAGE - 1))) as usize).min(bytes.len() - done);
            let phys = self.check(at, access).expect("validated above");
            // SAFETY: `phys..phys+chunk` lies in one frame owned by this address space, reached
            // through the HHDM; `bytes` is a separate kernel buffer.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes[done..].as_ptr(),
                    hhdm::<u8>(phys),
                    chunk,
                );
            }
            done += chunk;
        }
        true
    }

    /// Copies `buf.len()` bytes from user address `va`. Fails without a partial copy unless the
    /// whole range passes the `access` check.
    pub fn read(&self, va: u64, buf: &mut [u8], access: Access) -> bool {
        if !self.validate(va, buf.len(), access) {
            return false;
        }
        let mut done = 0;
        while done < buf.len() {
            let at = va + done as u64;
            let chunk = ((PAGE - (at & (PAGE - 1))) as usize).min(buf.len() - done);
            let phys = self.check(at, access).expect("validated above");
            // SAFETY: as in `write`, with the copy direction reversed.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    hhdm::<u8>(phys) as *const u8,
                    buf[done..].as_mut_ptr(),
                    chunk,
                );
            }
            done += chunk;
        }
        true
    }

    /// Reads one little-endian `u64` from user memory (kernel access; for tests and diagnostics).
    pub fn read_u64(&self, va: u64) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read(va, &mut b, Access::Kernel)
            .then(|| u64::from_le_bytes(b))
    }
}

impl crate::elf::SegmentMapper for AddressSpace {
    fn map_zeroed(
        &mut self,
        virtual_range: core::ops::Range<u64>,
        flags: u32,
    ) -> Result<(), crate::elf::Error> {
        let write = flags & 2 != 0;
        let exec = flags & 1 != 0;
        let mut va = virtual_range.start;
        while va < virtual_range.end {
            AddressSpace::map_zeroed(self, va, write, exec)
                .map_err(|_| crate::elf::Error::MappingFailed)?;
            va += PAGE;
        }
        Ok(())
    }

    fn copy(&mut self, virtual_address: u64, bytes: &[u8]) -> Result<(), crate::elf::Error> {
        if self.write(virtual_address, bytes, Access::Kernel) {
            Ok(())
        } else {
            Err(crate::elf::Error::MappingFailed)
        }
    }
}

fn free_table(table: PhysAddr, level: usize) {
    // SAFETY: `table` is a page table owned by the address space being dropped, reached through
    // the HHDM; nothing else references the lower half's tables.
    let entries = unsafe { &*hhdm::<PageTable>(table) };
    let limit = if level == 4 { 256 } else { 512 };
    for entry in entries.iter().take(limit) {
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        if level == 1 {
            frame::free(Frame::new(entry.addr()).expect("page-table entries are aligned"));
        } else {
            free_table(entry.addr(), level - 1);
        }
    }
    if level != 4 {
        frame::free(Frame::new(table).expect("tables are aligned"));
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        let (active, flags) = Cr3::read();
        if active == self.root {
            // SAFETY: the kernel's own table maps the whole kernel half, which is all the code
            // running here needs.
            unsafe { Cr3::write(paging::kernel_root(), flags) };
        }
        free_table(self.root.start_address(), 4);
        frame::free(Frame::new(self.root.start_address()).expect("root is aligned"));
    }
}

//! Per-thread kernel stacks (P2.5).
//!
//! Every thread that can enter ring 3 owns a kernel stack: syscalls, and interrupts and
//! exceptions taken in user mode, run on it, and [`crate::scheduler`] parks the thread's kernel
//! context there while it is switched out. Stacks live in their own window of the kernel dynamic
//! region, one fixed-size slot each with an unmapped guard page at the bottom, so an overflow
//! faults (and escalates to the double-fault handler) instead of corrupting a neighbour. Slots
//! are recycled when a stack is dropped.

use alloc::vec::Vec;

use x86_64::VirtAddr;
use x86_64::structures::paging::Page;

use super::{frame, paging};
use crate::sync::{SpinLock, without_interrupts};

/// Start of the kernel-stack window: 1 GiB into the dynamic region.
pub const KSTACK_BASE: u64 = paging::KERNEL_DYNAMIC_BASE + 0x4000_0000;
/// Mapped pages per stack (32 KiB); debug builds of the dispatcher need the headroom.
pub const KSTACK_PAGES: usize = 8;
/// Bytes of address space per slot: the guard page, the stack, and spare unmapped room.
const SLOT_SIZE: u64 = 64 * 1024;
/// Most stacks alive at once (a 256 MiB window).
const MAX_SLOTS: usize = 4096;

struct Slots {
    next: usize,
    free: Vec<usize>,
}

static SLOTS: SpinLock<Slots> = SpinLock::new(Slots {
    next: 0,
    free: Vec::new(),
});

/// A mapped kernel stack with a guard page below it; unmapped and freed on drop.
#[derive(Debug)]
pub struct KernelStack {
    slot: usize,
}

impl KernelStack {
    /// Maps a fresh stack. `None` if frames or slots run out.
    pub fn new() -> Option<Self> {
        let slot = without_interrupts(|| {
            let mut s = SLOTS.lock();
            if let Some(slot) = s.free.pop() {
                Some(slot)
            } else if s.next < MAX_SLOTS {
                s.next += 1;
                Some(s.next - 1)
            } else {
                None
            }
        })?;
        let stack = Self { slot };
        for i in 0..KSTACK_PAGES {
            let page = Page::containing_address(VirtAddr::new(stack.bottom() + (i as u64) * 4096));
            let Some(f) = frame::allocate() else {
                stack.release(i);
                return None;
            };
            if let Err((_, f)) = paging::map(page, f, paging::KERNEL_DATA) {
                frame::free(f);
                stack.release(i);
                return None;
            }
        }
        Some(stack)
    }

    /// Lowest mapped address (the guard page is the page below it).
    pub fn bottom(&self) -> u64 {
        KSTACK_BASE + self.slot as u64 * SLOT_SIZE + 4096
    }

    /// One past the highest mapped byte; 16-byte aligned.
    pub fn top(&self) -> u64 {
        self.bottom() + (KSTACK_PAGES as u64) * 4096
    }

    /// Unmaps the first `pages` pages and returns the slot. Consumes the stack.
    fn release(self, pages: usize) {
        let me = core::mem::ManuallyDrop::new(self);
        me.unmap(pages);
    }

    fn unmap(&self, pages: usize) {
        for i in 0..pages {
            let page = Page::containing_address(VirtAddr::new(self.bottom() + (i as u64) * 4096));
            match paging::unmap(page) {
                Ok(f) => frame::free(f),
                Err(e) => panic!("unmapping kernel stack page: {e}"),
            }
        }
        let slot = self.slot;
        without_interrupts(|| SLOTS.lock().free.push(slot));
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        self.unmap(KSTACK_PAGES);
    }
}

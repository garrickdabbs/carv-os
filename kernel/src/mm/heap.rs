//! Kernel heap (P1.4): `linked_list_allocator` behind the kernel spin lock, backing `alloc`.
//!
//! The heap lives at [`HEAP_BASE`] inside the kernel dynamic region and is backed by
//! [`HEAP_SIZE`] bytes of freshly allocated frames mapped at boot. The lock is taken with
//! interrupts disabled so an interrupt handler can never deadlock against an allocation in
//! progress. Every allocation will charge a Budget once budgets exist (P2.4); until then the heap
//! counts bytes in use so that accounting has something to hook into (`stats`).

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};

use linked_list_allocator::Heap;
use x86_64::VirtAddr;
use x86_64::structures::paging::Page;

use super::{frame, paging};
use crate::sync::{SpinLock, without_interrupts};

/// Start of the kernel heap: 256 MiB into the dynamic region, clear of the test and stack pages
/// used near its base.
pub const HEAP_BASE: u64 = paging::KERNEL_DYNAMIC_BASE + 0x1000_0000;

/// Heap size in bytes (256 frames). Grows in a later task when the kernel needs more.
pub const HEAP_SIZE: usize = 1024 * 1024;

struct KernelHeap {
    inner: SpinLock<Heap>,
    /// Bytes handed out and not yet returned (sum of requested layout sizes).
    in_use: AtomicUsize,
}

#[global_allocator]
static HEAP: KernelHeap = KernelHeap {
    inner: SpinLock::new(Heap::empty()),
    in_use: AtomicUsize::new(0),
};

// SAFETY: `Heap` hands out non-overlapping blocks from the mapped range and the lock (taken with
// interrupts off) serialises every call, so the `GlobalAlloc` contract holds.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        without_interrupts(|| match self.inner.lock().allocate_first_fit(layout) {
            Ok(p) => {
                self.in_use.fetch_add(layout.size(), Ordering::Relaxed);
                p.as_ptr()
            }
            Err(()) => ptr::null_mut(),
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        without_interrupts(|| {
            // SAFETY: `GlobalAlloc::dealloc` requires `ptr` to have come from `alloc` with this
            // `layout`, so it is non-null and owned by this heap.
            unsafe {
                self.inner
                    .lock()
                    .deallocate(NonNull::new_unchecked(ptr), layout);
            }
            self.in_use.fetch_sub(layout.size(), Ordering::Relaxed);
        });
    }
}

/// Heap usage snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Bytes the allocator has carved out (including block headers and alignment padding).
    pub used: usize,
    /// Bytes still available.
    pub free: usize,
    /// Sum of the sizes of live allocations as requested by callers.
    pub in_use: usize,
    /// Total heap size.
    pub size: usize,
}

/// Maps the heap's frames and initialises the allocator. Call once, after
/// [`super::paging::init`]; `alloc` is unusable before this.
///
/// # Panics
/// If the frames cannot be allocated or mapped: a kernel that cannot build its heap cannot run.
pub fn init() -> Stats {
    let pages = HEAP_SIZE / 4096;
    for i in 0..pages {
        let page = Page::containing_address(VirtAddr::new(HEAP_BASE + (i * 4096) as u64));
        let f = frame::allocate().expect("out of frames while mapping the kernel heap");
        if let Err((e, returned)) = paging::map(page, f, paging::KERNEL_DATA) {
            frame::free(returned);
            panic!("mapping kernel heap page {:#x}: {e}", page.start_address());
        }
    }
    without_interrupts(|| {
        // SAFETY: `[HEAP_BASE, HEAP_BASE + HEAP_SIZE)` was just mapped to frames owned by the
        // heap alone, is writable, and nothing else references it.
        unsafe { HEAP.inner.lock().init(HEAP_BASE as *mut u8, HEAP_SIZE) };
    });
    stats()
}

/// Current heap usage.
pub fn stats() -> Stats {
    without_interrupts(|| {
        let h = HEAP.inner.lock();
        Stats {
            used: h.used(),
            free: h.free(),
            in_use: HEAP.in_use.load(Ordering::Relaxed),
            size: h.size(),
        }
    })
}

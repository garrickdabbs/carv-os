//! Kernel heap (P1.4): `linked_list_allocator` behind the kernel spin lock, backing `alloc`.
//!
//! The heap lives at [`HEAP_BASE`] inside the kernel dynamic region and is backed by
//! [`HEAP_SIZE`] bytes of freshly allocated frames mapped at boot. The lock is taken with
//! interrupts disabled so an interrupt handler can never deadlock against an allocation in
//! progress. Every allocation charges the kernel heap's own [`Budget`] (memory limit
//! [`HEAP_SIZE`]) and is refused when that charge fails; frees credit it back. Kernel objects
//! created on behalf of user space are additionally charged to the caller's budget by the object
//! registry (`objects.rs`), so a process cannot exhaust the heap beyond what its budget allows.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};

use carv_budget::Budget;
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

struct HeapState {
    heap: Heap,
    /// Charged with the requested size of every live allocation; `None` until [`init`].
    budget: Option<Budget>,
}

struct KernelHeap {
    inner: SpinLock<HeapState>,
}

#[global_allocator]
static HEAP: KernelHeap = KernelHeap {
    inner: SpinLock::new(HeapState {
        heap: Heap::empty(),
        budget: None,
    }),
};

// SAFETY: `Heap` hands out non-overlapping blocks from the mapped range and the lock (taken with
// interrupts off) serialises every call, so the `GlobalAlloc` contract holds.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        without_interrupts(|| {
            let mut state = self.inner.lock();
            let Some(budget) = state.budget.as_mut() else {
                return ptr::null_mut();
            };
            if budget.charge_memory(layout.size() as u64).is_err() {
                return ptr::null_mut();
            }
            match state.heap.allocate_first_fit(layout) {
                Ok(p) => p.as_ptr(),
                Err(()) => {
                    if let Some(b) = state.budget.as_mut() {
                        let _ = b.release_memory(layout.size() as u64);
                    }
                    ptr::null_mut()
                }
            }
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        without_interrupts(|| {
            let mut state = self.inner.lock();
            // SAFETY: `GlobalAlloc::dealloc` requires `ptr` to have come from `alloc` with this
            // `layout`, so it is non-null and owned by this heap.
            unsafe { state.heap.deallocate(NonNull::new_unchecked(ptr), layout) };
            if let Some(b) = state.budget.as_mut() {
                let _ = b.release_memory(layout.size() as u64);
            }
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
    /// Sum of the sizes of live allocations as requested by callers (the heap budget's charge).
    pub in_use: usize,
    /// Memory limit of the heap budget.
    pub limit: usize,
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
        let mut state = HEAP.inner.lock();
        // SAFETY: `[HEAP_BASE, HEAP_BASE + HEAP_SIZE)` was just mapped to frames owned by the
        // heap alone, is writable, and nothing else references it.
        unsafe { state.heap.init(HEAP_BASE as *mut u8, HEAP_SIZE) };
        // No CPU allowance: this budget only accounts for heap memory.
        state.budget =
            Some(Budget::new(0, 1, HEAP_SIZE as u64, 0).expect("a zero CPU allowance is valid"));
    });
    stats()
}

/// Current heap usage.
pub fn stats() -> Stats {
    without_interrupts(|| {
        let state = HEAP.inner.lock();
        let (in_use, limit) = state.budget.as_ref().map_or((0, 0), |b| {
            (
                b.memory_used_bytes() as usize,
                b.limits().memory_bytes() as usize,
            )
        });
        Stats {
            used: state.heap.used(),
            free: state.heap.free(),
            in_use,
            limit,
            size: state.heap.size(),
        }
    })
}

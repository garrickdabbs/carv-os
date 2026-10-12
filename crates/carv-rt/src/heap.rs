//! A user heap that grows by mapping frames, one page at a time, from the process's budget.
//!
//! [`GrowHeap`] is the pure logic: a `linked_list_allocator` heap over a reserved virtual range
//! that starts empty and is extended whenever an allocation does not fit. The pages themselves
//! come from a [`PageMapper`]; in a program that is the `ADDRESS_SPACE_MAP` method, which charges
//! the address space's budget, so a program can never allocate more than its budget pays for —
//! when the budget is used up the mapper refuses and the allocation fails.

use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::NonNull;

use linked_list_allocator::Heap;

/// Page size in bytes.
pub const PAGE_SIZE: usize = 4096;
/// First address of the heap range. It lies above the program image (linked at 4 MiB) and far
/// below the stack ([`carv_abi::init::STACK_TOP`]).
pub const HEAP_BASE: usize = 0x4000_0000;
/// Largest the heap can ever grow, in bytes (1 GiB); the budget is the real limit.
pub const HEAP_MAX_BYTES: usize = 0x4000_0000;
/// Pages mapped at a time, at least, so that small allocations do not each cost a syscall.
pub const MIN_GROWTH_PAGES: usize = 16;

/// How many times one allocation may grow the heap before giving up.
const GROW_ATTEMPTS: usize = 3;
/// Slack added to a request so the allocator's block headers and alignment padding fit.
const OVERHEAD: usize = 4 * size_of::<usize>();

/// Source of zeroed, writable pages for a [`GrowHeap`].
///
/// # Safety
/// When `map_page` returns `true`, the 4 KiB page at `va` must be mapped read-write, readable as
/// zeroes, and used by nothing else for the rest of the program's life.
pub unsafe trait PageMapper {
    /// Maps a fresh zeroed page at the page-aligned address `va`; `false` if it cannot.
    fn map_page(&mut self, va: usize) -> bool;
}

/// A heap over `[base, base + max_bytes)` that maps its pages on demand.
pub struct GrowHeap<M> {
    heap: Heap,
    mapper: M,
    base: usize,
    max_bytes: usize,
    mapped: usize,
    initialized: bool,
}

impl<M: PageMapper> GrowHeap<M> {
    /// Creates an empty heap; no page is mapped until the first allocation.
    ///
    /// # Safety
    /// `[base, base + max_bytes)` must be page-aligned, reserved for this heap alone, and made
    /// accessible only through `mapper`.
    pub const unsafe fn new(mapper: M, base: usize, max_bytes: usize) -> Self {
        Self {
            heap: Heap::empty(),
            mapper,
            base,
            max_bytes,
            mapped: 0,
            initialized: false,
        }
    }

    /// Allocates a block for `layout`, mapping more pages if needed. Returns null when the range
    /// or the mapper (the budget) is exhausted.
    pub fn allocate(&mut self, layout: Layout) -> *mut u8 {
        for attempt in 0..=GROW_ATTEMPTS {
            if self.initialized
                && let Ok(p) = self.heap.allocate_first_fit(layout)
            {
                return p.as_ptr();
            }
            if attempt == GROW_ATTEMPTS {
                break;
            }
            let needed = layout.size().saturating_add(layout.align()) + OVERHEAD;
            let pages = needed.div_ceil(PAGE_SIZE).max(MIN_GROWTH_PAGES);
            if self.grow(pages) == 0 {
                break;
            }
        }
        core::ptr::null_mut()
    }

    /// Returns a block to the heap. Pages are never unmapped.
    ///
    /// # Safety
    /// `ptr` must have been returned by [`GrowHeap::allocate`] on this heap with this `layout`
    /// and not freed since.
    pub unsafe fn deallocate(&mut self, ptr: *mut u8, layout: Layout) {
        if let Some(p) = NonNull::new(ptr) {
            // SAFETY: the caller guarantees `p` is a live block of this heap with this layout.
            unsafe { self.heap.deallocate(p, layout) };
        }
    }

    /// Bytes mapped so far.
    pub fn mapped_bytes(&self) -> usize {
        self.mapped
    }

    /// Bytes currently handed out (including allocator bookkeeping).
    pub fn used_bytes(&self) -> usize {
        if self.initialized {
            self.heap.used()
        } else {
            0
        }
    }

    /// The page mapper.
    pub fn mapper(&self) -> &M {
        &self.mapper
    }

    /// Maps up to `pages` more pages at the end of the heap and hands them to the allocator.
    /// Returns how many pages were added (0 when the range is full or the mapper refuses).
    fn grow(&mut self, pages: usize) -> usize {
        let pages = pages.min((self.max_bytes - self.mapped) / PAGE_SIZE);
        let start = self.base + self.mapped;
        let mut added = 0;
        while added < pages && self.mapper.map_page(start + added * PAGE_SIZE) {
            added += 1;
        }
        if added > 0 {
            let bytes = added * PAGE_SIZE;
            if self.initialized {
                // SAFETY: the `bytes` bytes directly after the heap's current end were just
                // mapped by the mapper, and belong to this heap alone (see `new`).
                unsafe { self.heap.extend(bytes) };
            } else {
                // SAFETY: `[start, start + bytes)` was just mapped by the mapper, belongs to this
                // heap alone (see `new`), and the heap is empty and uninitialised.
                unsafe { self.heap.init(start as *mut u8, bytes) };
                self.initialized = true;
            }
            self.mapped += bytes;
        }
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{alloc_zeroed, dealloc};

    /// Host stand-in for the kernel: a zeroed arena where "mapping" a page checks that pages come
    /// in order and counts them, refusing after `budget` pages.
    struct Arena {
        base: usize,
        pages: usize,
        budget: usize,
        mapped: usize,
    }

    impl Arena {
        fn layout(pages: usize) -> Layout {
            Layout::from_size_align(pages * PAGE_SIZE, PAGE_SIZE).unwrap()
        }

        fn new(pages: usize, budget: usize) -> Self {
            // SAFETY: the layout has a nonzero size.
            let base = unsafe { alloc_zeroed(Self::layout(pages)) } as usize;
            assert_ne!(base, 0);
            Self {
                base,
                pages,
                budget,
                mapped: 0,
            }
        }
    }

    impl Drop for Arena {
        fn drop(&mut self) {
            // SAFETY: allocated in `new` with the same layout.
            unsafe { dealloc(self.base as *mut u8, Self::layout(self.pages)) };
        }
    }

    // SAFETY: pages handed out are inside a zeroed, writable arena owned by the mapper, and each
    // is handed out once (the test heap maps them strictly in order).
    unsafe impl PageMapper for &mut Arena {
        fn map_page(&mut self, va: usize) -> bool {
            assert_eq!(
                va,
                self.base + self.mapped * PAGE_SIZE,
                "pages must be contiguous"
            );
            if self.mapped == self.budget {
                return false;
            }
            self.mapped += 1;
            true
        }
    }

    fn heap(arena: &mut Arena) -> GrowHeap<&mut Arena> {
        let (base, bytes) = (arena.base, arena.pages * PAGE_SIZE);
        // SAFETY: the arena is reserved for this heap and only reached through its mapper.
        unsafe { GrowHeap::new(arena, base, bytes) }
    }

    #[test]
    fn nothing_is_mapped_until_the_first_allocation() {
        let mut arena = Arena::new(64, 64);
        let mut h = heap(&mut arena);
        assert_eq!(h.mapped_bytes(), 0);
        assert_eq!(h.used_bytes(), 0);
        let p = h.allocate(Layout::new::<u64>());
        assert!(!p.is_null());
        assert_eq!(h.mapped_bytes(), MIN_GROWTH_PAGES * PAGE_SIZE);
        assert_eq!(h.mapper().mapped, MIN_GROWTH_PAGES);
    }

    #[test]
    fn allocations_are_aligned_and_distinct() {
        let mut arena = Arena::new(64, 64);
        let mut h = heap(&mut arena);
        let mut blocks = std::vec::Vec::new();
        for align in [1usize, 8, 64, 4096] {
            let layout = Layout::from_size_align(100, align).unwrap();
            let p = h.allocate(layout);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0);
            blocks.push((p, layout));
        }
        for (i, (p, _)) in blocks.iter().enumerate() {
            for (q, _) in &blocks[i + 1..] {
                assert!((*p as usize).abs_diff(*q as usize) >= 100);
            }
        }
        for (p, layout) in blocks {
            // SAFETY: each block was allocated above with this layout.
            unsafe { h.deallocate(p, layout) };
        }
    }

    #[test]
    fn heap_grows_to_fit_a_large_allocation_and_reuses_freed_memory() {
        let mut arena = Arena::new(256, 256);
        let mut h = heap(&mut arena);
        let big = Layout::from_size_align(100 * PAGE_SIZE, 8).unwrap();
        let p = h.allocate(big);
        assert!(!p.is_null());
        assert!(h.mapped_bytes() >= 100 * PAGE_SIZE);
        let mapped = h.mapped_bytes();
        // SAFETY: `p` came from `allocate` with `big`.
        unsafe { h.deallocate(p, big) };
        let again = h.allocate(big);
        assert!(!again.is_null());
        assert_eq!(
            h.mapped_bytes(),
            mapped,
            "freed memory is reused, not remapped"
        );
    }

    #[test]
    fn allocation_fails_cleanly_when_the_budget_is_used_up() {
        let mut arena = Arena::new(256, 20);
        let mut h = heap(&mut arena);
        assert!(!h.allocate(Layout::from_size_align(8, 8).unwrap()).is_null());
        let too_big = Layout::from_size_align(64 * PAGE_SIZE, 8).unwrap();
        assert!(h.allocate(too_big).is_null());
        // The heap is still usable, and the pages that were granted were not lost.
        assert!(!h.allocate(Layout::from_size_align(8, 8).unwrap()).is_null());
        assert!(h.mapped_bytes() <= 20 * PAGE_SIZE);
    }

    #[test]
    fn allocation_fails_when_the_reserved_range_is_full() {
        let mut arena = Arena::new(64, 64);
        let base = arena.base;
        // SAFETY: the arena is reserved for this heap; the range is limited to 32 pages of it.
        let mut h = unsafe { GrowHeap::new(&mut arena, base, 32 * PAGE_SIZE) };
        let p = h.allocate(Layout::from_size_align(16 * PAGE_SIZE, 8).unwrap());
        assert!(!p.is_null());
        assert!(
            h.allocate(Layout::from_size_align(32 * PAGE_SIZE, 8).unwrap())
                .is_null()
        );
        assert_eq!(h.mapped_bytes(), 32 * PAGE_SIZE);
    }

    #[test]
    fn many_small_allocations_trigger_repeated_growth() {
        let mut arena = Arena::new(128, 128);
        let mut h = heap(&mut arena);
        let layout = Layout::from_size_align(1024, 8).unwrap();
        for _ in 0..200 {
            assert!(!h.allocate(layout).is_null());
        }
        assert!(h.mapped_bytes() >= 200 * 1024);
        assert!(h.used_bytes() >= 200 * 1024);
    }
}

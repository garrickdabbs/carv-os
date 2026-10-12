//! Program start-up and shutdown: the entry point glue, the global allocator, exit and the panic
//! handler.

use core::alloc::{GlobalAlloc, Layout};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use carv_abi::{init, map};

use crate::heap::{GrowHeap, HEAP_BASE, HEAP_MAX_BYTES, PageMapper};
use crate::sync::SpinLock;
use crate::{println, sys};

/// Maps heap pages into the program's own address space (`init::ADDRESS_SPACE`), which charges
/// the address space's budget.
struct SyscallMapper;

// SAFETY: `ADDRESS_SPACE_MAP` installs a fresh zeroed writable frame at the page; the heap range
// is reserved for the heap alone (nothing else in the runtime or a program maps below
// `HEAP_MAX_BYTES` from `HEAP_BASE`), so the page is not used by anything else.
unsafe impl PageMapper for SyscallMapper {
    fn map_page(&mut self, va: usize) -> bool {
        sys::map_page(init::ADDRESS_SPACE, va as u64, map::WRITE).is_ok()
    }
}

struct Allocator {
    heap: SpinLock<GrowHeap<SyscallMapper>>,
}

// SAFETY: the heap's blocks are handed out exclusively and every call goes through the lock, so
// the `GlobalAlloc` contract (distinct live blocks, thread-safe) holds. `Sync` is needed because
// the heap holds raw pointers into the mapped range; they are only used under the lock.
unsafe impl Sync for Allocator {}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator {
    // SAFETY: `GrowHeap::new` needs its range reserved for the heap alone and reached only through
    // the mapper: `[HEAP_BASE, HEAP_BASE + HEAP_MAX_BYTES)` is page aligned, above the program image
    // and below the stack, and only `SyscallMapper` maps it.
    heap: SpinLock::new(unsafe { GrowHeap::new(SyscallMapper, HEAP_BASE, HEAP_MAX_BYTES) }),
};

// SAFETY: see `Allocator`; `alloc` returns null on exhaustion as the trait requires, and `dealloc`
// receives only blocks from `alloc` with the same layout.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.heap.lock().allocate(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees `ptr` is a live block from `alloc` with `layout`.
        unsafe { self.heap.lock().deallocate(ptr, layout) }
    }
}

/// Runs `main` and exits. This is what [`entry!`](crate::entry) calls from `_start`.
pub fn start(main: fn()) -> ! {
    main();
    exit()
}

/// Ends the calling thread by destroying its thread capability, which the kernel installs in
/// slot [`init::THREAD`] of every process it creates.
pub fn exit() -> ! {
    exit_thread(init::THREAD)
}

/// Ends the calling thread, whose thread capability is in slot `thread`. If that fails the thread
/// parks forever rather than return into code that has finished.
pub fn exit_thread(thread: u64) -> ! {
    let _ = sys::destroy(thread);
    loop {
        sys::yield_now();
    }
}

static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    // A panic while printing the panic (e.g. in a Display impl) must not recurse.
    if !PANICKING.swap(true, Ordering::Relaxed) {
        println!("panic: {info}");
    }
    exit()
}

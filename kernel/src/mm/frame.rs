//! Physical frame allocator built from the Limine memory map.
//!
//! The bitmap itself is carved out of the first usable region large enough to hold it and is
//! reached through the higher-half direct map (HHDM), so this module needs no paging code of its
//! own. All logic lives in the pure `carv-frames` crate; this file only maps Limine's view of
//! memory onto it and guards the allocator with a lock. Every lock acquisition runs with
//! interrupts disabled (`SpinLock` is not interrupt-safe on its own) so a future interrupt handler
//! that needs a frame can never deadlock against a preempted holder.

use carv_frames::{BitmapAllocator, FRAME_SIZE, storage_words};
use limine::memmap::{Entry, MEMMAP_USABLE};
use x86_64::PhysAddr;

use crate::sync::{SpinLock, without_interrupts};

/// A 4 KiB physical frame, identified by its base address. Only constructible for frame-aligned
/// addresses; freeing one that this allocator did not hand out is rejected at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame(PhysAddr);

impl Frame {
    /// Wraps `addr` if it is 4 KiB aligned.
    pub fn new(addr: PhysAddr) -> Option<Self> {
        addr.as_u64()
            .is_multiple_of(FRAME_SIZE as u64)
            .then_some(Frame(addr))
    }

    /// Frame for bitmap index `index`.
    pub fn from_index(index: usize) -> Self {
        Frame(PhysAddr::new((index * FRAME_SIZE) as u64))
    }

    /// Physical base address.
    pub fn start(self) -> PhysAddr {
        self.0
    }

    /// Bitmap index (`base / FRAME_SIZE`).
    pub fn index(self) -> usize {
        (self.0.as_u64() / FRAME_SIZE as u64) as usize
    }
}

/// Where the allocator put its bitmap and what it covers; printed at boot.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    /// Physical address of the bitmap storage.
    pub bitmap_phys: PhysAddr,
    /// Bitmap storage size in bytes (used map + allocated map).
    pub bitmap_bytes: usize,
    /// Frames covered by the bitmap (up to the highest usable address).
    pub frame_count: usize,
    /// Usable frames after the bitmap's own frames were reserved.
    pub free_frames: usize,
}

static ALLOCATOR: SpinLock<Option<BitmapAllocator<'static>>> = SpinLock::new(None);

/// Runs `f` with the allocator locked and interrupts disabled.
fn with_allocator<R>(f: impl FnOnce(&mut BitmapAllocator<'static>) -> R) -> R {
    without_interrupts(|| {
        let mut guard = ALLOCATOR.lock();
        f(guard.as_mut().expect("frame allocator not initialised"))
    })
}

/// Builds the allocator from Limine's memory map. `hhdm_offset` is the higher-half direct map
/// base, so physical address `p` is readable at virtual `hhdm_offset + p`.
///
/// # Panics
/// If no usable region can hold the bitmap (an unrealistically small machine).
pub fn init(entries: &[&Entry], hhdm_offset: u64) -> Layout {
    let usable = entries.iter().copied().filter(|e| e.type_ == MEMMAP_USABLE);
    let top = usable
        .clone()
        .map(|e| e.base + e.length)
        .max()
        .expect("Limine memory map has no usable region");
    let frame_count = (top / FRAME_SIZE as u64) as usize;
    let words = storage_words(frame_count);
    let bitmap_bytes = words * 8;
    let bitmap_frames = bitmap_bytes.div_ceil(FRAME_SIZE);

    // Limine guarantees usable entries are 4 KiB aligned and non-overlapping; take the first one
    // big enough to hold the bitmap.
    let home = usable
        .clone()
        .find(|e| e.length as usize >= bitmap_frames * FRAME_SIZE)
        .expect("no usable region large enough for the frame bitmap");
    let bitmap_phys = PhysAddr::new(home.base);

    // SAFETY: `home` is usable RAM the bootloader handed to us and nothing else has claimed it;
    // the HHDM maps all physical memory, so the pointer is valid for `words` u64s, and we mark the
    // frames it occupies as used below so the allocator never hands them out.
    let bits: &'static mut [u64] =
        unsafe { core::slice::from_raw_parts_mut((hhdm_offset + home.base) as *mut u64, words) };
    let mut alloc = BitmapAllocator::new(bits, frame_count);
    for e in usable {
        alloc.free_range(
            (e.base / FRAME_SIZE as u64) as usize,
            (e.length / FRAME_SIZE as u64) as usize,
        );
    }
    alloc.mark_used_range((home.base / FRAME_SIZE as u64) as usize, bitmap_frames);
    // Never hand out frame 0: a null physical address is too easy to mistake for "no frame".
    alloc.mark_used_range(0, 1);

    let layout = Layout {
        bitmap_phys,
        bitmap_bytes,
        frame_count,
        free_frames: alloc.free_frames(),
    };
    without_interrupts(|| *ALLOCATOR.lock() = Some(alloc));
    layout
}

/// Allocates one frame. `None` when memory is exhausted.
pub fn allocate() -> Option<Frame> {
    with_allocator(|a| a.allocate()).map(Frame::from_index)
}

/// Returns a frame obtained from [`allocate`]. Frames that were never handed out — including
/// reservations such as frame 0 and the bitmap's own storage — are rejected.
///
/// # Panics
/// On double free, reserved or out-of-range frames: all are kernel bugs, not runtime conditions.
pub fn free(frame: Frame) {
    if let Err(e) = with_allocator(|a| a.free(frame.index())) {
        panic!("frame::free({:#x}): {e:?}", frame.0.as_u64());
    }
}

/// Boot-time self-check: allocate one frame, write and read a pattern through the HHDM, free it,
/// and confirm the free count is unchanged. Returns the frame that was used.
///
/// # Panics
/// If any step disagrees — a broken allocator is not something to boot past.
pub fn self_check(hhdm_offset: u64) -> Frame {
    let (free_before, _) = stats();
    let frame = allocate().expect("frame allocator has no free frame at boot");
    assert_ne!(frame.0.as_u64(), 0, "frame 0 handed out");
    let virt = (hhdm_offset + frame.0.as_u64()) as *mut u64;
    // SAFETY: the frame was just allocated to us and nothing else references it; the HHDM maps
    // every physical frame, so `virt` is a valid, writable, 8-byte-aligned pointer.
    unsafe {
        core::ptr::write_volatile(virt, 0xC0DE_CAFE_F00D_BEEF);
        assert_eq!(core::ptr::read_volatile(virt), 0xC0DE_CAFE_F00D_BEEF);
    }
    free(frame);
    assert_eq!(stats().0, free_before, "free count not restored");
    frame
}

/// `(free, total)` frame counts.
pub fn stats() -> (usize, usize) {
    with_allocator(|a| (a.free_frames(), a.frame_count()))
}

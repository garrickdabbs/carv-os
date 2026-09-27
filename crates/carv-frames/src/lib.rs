//! Bitmap allocator for 4 KiB physical frames.
//!
//! Pure logic over a caller-provided `&mut [u64]` bitmap, so the same code is unit-tested on the
//! host and used by the kernel (`kernel/src/mm/frame.rs`) with the bitmap living in a usable region
//! of the Limine memory map. Two bits per frame, kept in two equal halves of the slice:
//!
//! * **used** — `1` means the frame may not be handed out (reserved, MMIO, kernel image, or
//!   currently allocated). Frames outside the ranges the caller frees stay used forever.
//! * **allocated** — `1` means the frame was handed out by [`BitmapAllocator::allocate`].
//!   Only such frames can be returned with [`BitmapAllocator::free`], so a stray `free` can never
//!   turn a reservation (the bitmap's own storage, frame 0, firmware tables) into free memory.
//!
//! Budget accounting (docs/PLAN.md §3.3 D) wraps this in P2.4; this layer only tracks the raw
//! resource.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

/// Size of one physical frame in bytes.
pub const FRAME_SIZE: usize = 4096;

/// Number of `u64` words in *one* bit-per-frame map.
pub const fn bitmap_words(frame_count: usize) -> usize {
    frame_count.div_ceil(64)
}

/// Total `u64` words [`BitmapAllocator::new`] needs: the used map and the allocated map.
pub const fn storage_words(frame_count: usize) -> usize {
    2 * bitmap_words(frame_count)
}

/// Errors from [`BitmapAllocator::free`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The frame index is beyond `frame_count`.
    OutOfRange,
    /// The frame was not handed out by `allocate` (never allocated, already freed, or reserved).
    NotAllocated,
}

/// A bitmap of frames. Index `i` covers physical bytes `[i * FRAME_SIZE, (i + 1) * FRAME_SIZE)`.
pub struct BitmapAllocator<'a> {
    /// `[0, words)` = used map, `[words, 2 * words)` = allocated map.
    bits: &'a mut [u64],
    words: usize,
    frame_count: usize,
    free: usize,
    /// Word index to resume scanning from; purely a performance hint, always `< words`.
    hint: usize,
}

impl<'a> BitmapAllocator<'a> {
    /// Wraps the first [`storage_words`]`(frame_count)` words of `bits` with **every frame marked
    /// used** and nothing allocated. Words beyond that are left untouched. Call
    /// [`free_range`](Self::free_range) for each usable region afterwards.
    ///
    /// # Panics
    /// If `bits` is shorter than [`storage_words`]`(frame_count)`.
    pub fn new(bits: &'a mut [u64], frame_count: usize) -> Self {
        let words = bitmap_words(frame_count);
        assert!(bits.len() >= 2 * words, "bitmap storage too small");
        let (used, allocated) = bits[..2 * words].split_at_mut(words);
        used.fill(u64::MAX);
        allocated.fill(0);
        Self {
            bits,
            words,
            frame_count,
            free: 0,
            hint: 0,
        }
    }

    /// Total frames the bitmap covers.
    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Frames currently free.
    pub fn free_frames(&self) -> usize {
        self.free
    }

    fn used_bit(&self, i: usize) -> bool {
        self.bits[i / 64] & (1u64 << (i % 64)) != 0
    }

    fn allocated_bit(&self, i: usize) -> bool {
        self.bits[self.words + i / 64] & (1u64 << (i % 64)) != 0
    }

    fn set_used(&mut self, i: usize, on: bool) {
        let m = 1u64 << (i % 64);
        if on {
            self.bits[i / 64] |= m;
        } else {
            self.bits[i / 64] &= !m;
        }
    }

    fn set_allocated(&mut self, i: usize, on: bool) {
        let m = 1u64 << (i % 64);
        let w = self.words + i / 64;
        if on {
            self.bits[w] |= m;
        } else {
            self.bits[w] &= !m;
        }
    }

    /// Whether frame `index` is marked used (out-of-range frames count as used).
    pub fn is_used(&self, index: usize) -> bool {
        index >= self.frame_count || self.used_bit(index)
    }

    /// Whether frame `index` is currently handed out by [`allocate`](Self::allocate).
    pub fn is_allocated(&self, index: usize) -> bool {
        index < self.frame_count && self.allocated_bit(index)
    }

    /// Marks `count` frames starting at `first` as free (usable). Frames already free are left
    /// alone, so overlapping memory-map entries are harmless; frames beyond `frame_count` are
    /// ignored. Frames currently *allocated* are not touched either — freeing those is the job of
    /// [`free`](Self::free), which enforces ownership.
    pub fn free_range(&mut self, first: usize, count: usize) {
        let end = first.saturating_add(count).min(self.frame_count);
        for i in first..end {
            if self.used_bit(i) && !self.allocated_bit(i) {
                self.set_used(i, false);
                self.free += 1;
            }
        }
        // `min` can only lower the hint, so it can never point past the bitmap; the range check
        // just makes that explicit for readers (#38).
        if first < self.frame_count {
            self.hint = self.hint.min(first / 64);
        }
    }

    /// Reserves `count` frames starting at `first` (e.g. the bitmap's own storage). Frames that
    /// are currently allocated stay allocated; the reservation applies once they are freed.
    pub fn mark_used_range(&mut self, first: usize, count: usize) {
        let end = first.saturating_add(count).min(self.frame_count);
        for i in first..end {
            if !self.used_bit(i) {
                self.set_used(i, true);
                self.free -= 1;
            }
        }
    }

    /// Allocates one frame, lowest free index first (from the scan hint). `None` when exhausted.
    pub fn allocate(&mut self) -> Option<usize> {
        for pass in 0..2 {
            let (start, end) = if pass == 0 {
                (self.hint, self.words)
            } else {
                (0, self.hint)
            };
            for w in start..end {
                let word = self.bits[w];
                if word != u64::MAX {
                    let bit = (!word).trailing_zeros() as usize;
                    let index = w * 64 + bit;
                    if index >= self.frame_count {
                        // Padding bits past the end of the last word: nothing there.
                        continue;
                    }
                    self.set_used(index, true);
                    self.set_allocated(index, true);
                    self.free -= 1;
                    self.hint = w;
                    return Some(index);
                }
            }
        }
        None
    }

    /// Returns a frame obtained from [`allocate`](Self::allocate). Reserved frames and frames that
    /// were never (or are no longer) allocated are rejected with [`FrameError::NotAllocated`].
    pub fn free(&mut self, index: usize) -> Result<(), FrameError> {
        if index >= self.frame_count {
            return Err(FrameError::OutOfRange);
        }
        if !self.allocated_bit(index) {
            return Err(FrameError::NotAllocated);
        }
        self.set_allocated(index, false);
        self.set_used(index, false);
        self.free += 1;
        self.hint = self.hint.min(index / 64);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn fresh(frames: usize) -> (Vec<u64>, usize) {
        (vec![0u64; storage_words(frames)], frames)
    }

    #[test]
    fn starts_fully_used_then_frees_ranges() {
        let (mut bits, n) = fresh(200);
        let mut a = BitmapAllocator::new(&mut bits, n);
        assert_eq!(a.free_frames(), 0);
        assert!(a.allocate().is_none());
        a.free_range(10, 50);
        a.free_range(40, 30); // overlaps: only 20 new frames
        assert_eq!(a.free_frames(), 60);
        a.free_range(190, 100); // clipped at frame_count
        assert_eq!(a.free_frames(), 70);
        assert!(a.is_used(9) && !a.is_used(10) && !a.is_used(69) && a.is_used(70));
    }

    #[test]
    fn new_only_touches_its_own_storage() {
        let words = storage_words(100);
        let mut bits = vec![0xDEAD_BEEFu64; words + 3];
        let _a = BitmapAllocator::new(&mut bits, 100);
        assert!(
            bits[words..].iter().all(|&w| w == 0xDEAD_BEEF),
            "words past the bitmap were overwritten"
        );
    }

    #[test]
    fn allocate_and_free_ten_thousand_frames_no_duplicates_count_restored() {
        let (mut bits, n) = fresh(12_345);
        let mut a = BitmapAllocator::new(&mut bits, n);
        a.free_range(0, n);
        a.mark_used_range(0, 100); // pretend the bitmap lives in the first 100 frames
        let before = a.free_frames();
        assert_eq!(before, n - 100);

        let mut got: Vec<usize> = (0..10_000).map(|_| a.allocate().unwrap()).collect();
        assert_eq!(a.free_frames(), before - 10_000);
        let mut sorted = got.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 10_000, "duplicate frame handed out");
        assert!(
            sorted.iter().all(|&i| i >= 100 && i < n),
            "allocated a reserved or out-of-range frame"
        );
        for &i in &got {
            assert!(a.is_used(i) && a.is_allocated(i));
        }

        // Free in a scrambled order; count must come back exactly.
        got.reverse();
        for (k, &i) in got.iter().enumerate() {
            if k % 3 == 0 {
                a.free(i).unwrap();
            }
        }
        for (k, &i) in got.iter().enumerate() {
            if k % 3 != 0 {
                a.free(i).unwrap();
            }
        }
        assert_eq!(a.free_frames(), before);
        for &i in &got {
            assert!(!a.is_used(i) && !a.is_allocated(i));
        }
    }

    #[test]
    fn free_rejects_double_free_reserved_and_out_of_range() {
        let (mut bits, n) = fresh(64);
        let mut a = BitmapAllocator::new(&mut bits, n);
        a.free_range(0, n);
        a.mark_used_range(0, 4); // reserved: e.g. the bitmap's own frames
        let f = a.allocate().unwrap();
        assert_eq!(a.free(f), Ok(()));
        assert_eq!(a.free(f), Err(FrameError::NotAllocated), "double free");
        assert_eq!(
            a.free(0),
            Err(FrameError::NotAllocated),
            "reserved frame must stay reserved"
        );
        assert!(a.is_used(0), "reservation cleared by a bogus free");
        assert_eq!(a.free(64), Err(FrameError::OutOfRange));
        assert_eq!(a.free(usize::MAX), Err(FrameError::OutOfRange));
    }

    #[test]
    fn free_range_does_not_release_allocated_frames() {
        let (mut bits, n) = fresh(64);
        let mut a = BitmapAllocator::new(&mut bits, n);
        a.free_range(0, n);
        let f = a.allocate().unwrap();
        a.free_range(0, n); // a sloppy second pass over the same region
        assert!(
            a.is_allocated(f) && a.is_used(f),
            "free_range must not steal an allocated frame"
        );
        assert_eq!(a.free_frames(), n - 1);
    }

    #[test]
    fn exhausts_exactly_and_reuses_freed_frames() {
        let (mut bits, n) = fresh(130); // spans three words with padding bits
        let mut a = BitmapAllocator::new(&mut bits, n);
        a.free_range(0, n);
        let all: Vec<usize> = (0..n)
            .map(|_| a.allocate().expect("frame available"))
            .collect();
        assert!(
            a.allocate().is_none(),
            "padding bits must never be handed out"
        );
        assert_eq!(a.free_frames(), 0);
        a.free(all[77]).unwrap();
        assert_eq!(a.allocate(), Some(77));
    }

    #[test]
    fn out_of_range_free_range_is_harmless() {
        let (mut bits, n) = fresh(100);
        let mut a = BitmapAllocator::new(&mut bits, n);
        a.free_range(0, n);
        for _ in 0..90 {
            a.allocate().unwrap(); // moves the hint to the last word
        }
        a.free_range(1_000_000, 5); // entirely beyond the bitmap
        a.free_range(usize::MAX - 2, 10); // saturating end
        assert_eq!(a.free_frames(), 10);
        assert!(
            a.allocate().is_some(),
            "allocate must not index past the bitmap"
        );
    }

    #[test]
    fn hint_never_skips_lower_frames_after_free() {
        let (mut bits, n) = fresh(1024);
        let mut a = BitmapAllocator::new(&mut bits, n);
        a.free_range(0, n);
        for _ in 0..900 {
            a.allocate().unwrap();
        }
        a.free(3).unwrap();
        assert_eq!(a.allocate(), Some(3));
    }
}

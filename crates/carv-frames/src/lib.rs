//! Bitmap allocator for 4 KiB physical frames.
//!
//! Pure logic over a caller-provided `&mut [u64]` bitmap, so the same code is unit-tested on the
//! host and used by the kernel (`kernel/src/mm/frame.rs`) with the bitmap living in a usable region
//! of the Limine memory map. One bit per frame, `1` = used. Frames outside the ranges the caller
//! frees stay used forever, which is how reserved, MMIO and kernel regions are excluded.
//!
//! Budget accounting (docs/PLAN.md §3.3 D) wraps this in P2.4; this layer only tracks the raw
//! resource.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

/// Size of one physical frame in bytes.
pub const FRAME_SIZE: usize = 4096;

/// Number of `u64` words needed to hold one bit per frame.
pub const fn bitmap_words(frame_count: usize) -> usize {
    frame_count.div_ceil(64)
}

/// Errors from [`BitmapAllocator::free`] and the range operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The frame index is beyond `frame_count`.
    OutOfRange,
    /// The frame was already free (double free).
    AlreadyFree,
}

/// A bitmap of frames. Index `i` covers physical bytes `[i * FRAME_SIZE, (i + 1) * FRAME_SIZE)`.
pub struct BitmapAllocator<'a> {
    bits: &'a mut [u64],
    frame_count: usize,
    free: usize,
    /// Word index to resume scanning from; purely a performance hint.
    hint: usize,
}

impl<'a> BitmapAllocator<'a> {
    /// Wraps `bits` (at least [`bitmap_words`]`(frame_count)` long) with **every frame marked
    /// used**. Call [`free_range`](Self::free_range) for each usable region afterwards.
    ///
    /// # Panics
    /// If `bits` is too short.
    pub fn new(bits: &'a mut [u64], frame_count: usize) -> Self {
        assert!(bits.len() >= bitmap_words(frame_count), "bitmap too small");
        for w in bits.iter_mut() {
            *w = u64::MAX;
        }
        Self {
            bits,
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

    /// Whether frame `index` is marked used (out-of-range frames count as used).
    pub fn is_used(&self, index: usize) -> bool {
        if index >= self.frame_count {
            return true;
        }
        self.bits[index / 64] & (1u64 << (index % 64)) != 0
    }

    /// Marks `count` frames starting at `first` as free. Frames already free are left alone, so
    /// overlapping memory-map entries are harmless. Frames beyond `frame_count` are ignored.
    pub fn free_range(&mut self, first: usize, count: usize) {
        let end = first.saturating_add(count).min(self.frame_count);
        for i in first..end {
            if self.is_used(i) {
                self.bits[i / 64] &= !(1u64 << (i % 64));
                self.free += 1;
            }
        }
        self.hint = self.hint.min(first / 64);
    }

    /// Marks `count` frames starting at `first` as used (e.g. the bitmap's own storage).
    pub fn mark_used_range(&mut self, first: usize, count: usize) {
        let end = first.saturating_add(count).min(self.frame_count);
        for i in first..end {
            if !self.is_used(i) {
                self.bits[i / 64] |= 1u64 << (i % 64);
                self.free -= 1;
            }
        }
    }

    /// Allocates one frame, lowest free index first (from the scan hint). `None` when exhausted.
    pub fn allocate(&mut self) -> Option<usize> {
        let words = bitmap_words(self.frame_count);
        for pass in 0..2 {
            let (start, end) = if pass == 0 {
                (self.hint, words)
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
                    self.bits[w] |= 1u64 << bit;
                    self.free -= 1;
                    self.hint = w;
                    return Some(index);
                }
            }
        }
        None
    }

    /// Frees one frame previously returned by [`allocate`](Self::allocate).
    pub fn free(&mut self, index: usize) -> Result<(), FrameError> {
        if index >= self.frame_count {
            return Err(FrameError::OutOfRange);
        }
        if !self.is_used(index) {
            return Err(FrameError::AlreadyFree);
        }
        self.bits[index / 64] &= !(1u64 << (index % 64));
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
        (vec![0u64; bitmap_words(frames)], frames)
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
            assert!(a.is_used(i));
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
            assert!(!a.is_used(i));
        }
    }

    #[test]
    fn double_free_and_out_of_range_are_errors() {
        let (mut bits, n) = fresh(64);
        let mut a = BitmapAllocator::new(&mut bits, n);
        a.free_range(0, n);
        let f = a.allocate().unwrap();
        assert_eq!(a.free(f), Ok(()));
        assert_eq!(a.free(f), Err(FrameError::AlreadyFree));
        assert_eq!(a.free(64), Err(FrameError::OutOfRange));
        assert_eq!(a.free(usize::MAX), Err(FrameError::OutOfRange));
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

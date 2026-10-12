//! A minimal spin lock for the user heap and other runtime state shared between threads.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A test-and-set spin lock. On the target it yields to the scheduler while contended, so a
/// preempted lock holder gets the CPU back instead of being waited for until the slice ends.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the lock serialises every access to `value`, so sharing the lock between threads is
// sound whenever the protected value may be moved between them.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Creates an unlocked lock around `value`.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquires the lock, spinning until it is free.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            #[cfg(target_os = "none")]
            crate::sys::yield_now();
            spin_loop();
        }
        SpinGuard { lock: self }
    }
}

/// Exclusive access to the value inside a [`SpinLock`]; releases the lock when dropped.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: holding the guard means this thread holds the lock, so no `&mut` exists.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: holding the guard means this thread holds the lock and nothing else can
        // reach the value.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_gives_exclusive_mutable_access_and_releases_on_drop() {
        let lock = SpinLock::new(1u32);
        *lock.lock() += 41;
        assert_eq!(*lock.lock(), 42);
    }

    #[test]
    fn contended_counter_is_exact() {
        static LOCK: SpinLock<u64> = SpinLock::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..1000 {
                        *LOCK.lock() += 1;
                    }
                });
            }
        });
        assert_eq!(*LOCK.lock(), 4000);
    }
}

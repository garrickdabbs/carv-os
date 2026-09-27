//! Minimal synchronisation primitives for the single-core kernel.
//!
//! [`SpinLock`] is a plain test-and-set lock. It is *not* interrupt-safe on its own: a lock
//! taken by kernel code and then needed by an interrupt handler on the same CPU would
//! deadlock. Paths that can run with interrupts enabled wrap the lock in
//! [`without_interrupts`], and the panic path bypasses locks entirely.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::x86_64;

pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the lock serialises all access to `value`, so sharing the lock across threads is
// sound whenever `T` itself may be sent between them.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Spins until the lock is acquired.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinLockGuard { lock: self }
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: holding the guard means we hold the lock, so no other reference exists.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and `&mut self` guarantees this is the only guard-derived borrow.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// Runs `f` with interrupts disabled, restoring the previous interrupt state afterwards.
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let were_enabled = x86_64::interrupts_enabled();
    if were_enabled {
        x86_64::disable_interrupts();
    }
    let result = f();
    if were_enabled {
        x86_64::enable_interrupts();
    }
    result
}

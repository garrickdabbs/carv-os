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

/// A `static` with interior mutability for boot-time tables (GDT, TSS, IDT) that are written
/// exactly once during single-core initialisation and read-only afterwards. It performs no
/// synchronisation at all; every access is `unsafe` and the caller upholds that discipline.
pub struct StaticCell<T>(UnsafeCell<T>);

// SAFETY: access is serialised by construction (see the type docs); the cell adds no interior
// mutability beyond what the caller promises to manage.
unsafe impl<T: Sync> Sync for StaticCell<T> {}

impl<T> StaticCell<T> {
    /// Wraps `value`.
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    /// Shared access.
    ///
    /// # Safety
    /// No `get_mut` borrow may be live, and the value must already be initialised if readers
    /// depend on that.
    pub unsafe fn get(&self) -> &T {
        // SAFETY: the caller guarantees no concurrent mutable borrow.
        unsafe { &*self.0.get() }
    }

    /// Exclusive access.
    ///
    /// # Safety
    /// The caller must be the only accessor for the borrow's lifetime — in practice, boot-time
    /// initialisation on the boot CPU with interrupts disabled.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> &mut T {
        // SAFETY: the caller guarantees exclusivity.
        unsafe { &mut *self.0.get() }
    }

    /// Raw pointer to the value, for address arithmetic without creating a reference.
    pub fn as_ptr(&self) -> *const T {
        self.0.get()
    }
}

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

//! The lock around the sample queue, taken by the real-time output callback.
//!
//! `std::sync::Mutex` is a plain pthread mutex on macOS: when the callback
//! waits on one held by a normal-priority thread that the scheduler has
//! preempted, the audio thread misses its deadline and CoreAudio reports an
//! overload. `os_unfair_lock` lends the waiter's priority to the holder, so the
//! holder finishes its few instructions at real-time priority instead.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

pub struct RtMutex<T> {
    #[cfg(target_os = "macos")]
    lock: UnsafeCell<libc::os_unfair_lock>,
    #[cfg(not(target_os = "macos"))]
    lock: std::sync::Mutex<()>,
    value: UnsafeCell<T>,
}

// SAFETY: `value` is only reached through a guard, and one guard exists at a time.
unsafe impl<T: Send> Send for RtMutex<T> {}
// SAFETY: as above; the lock serialises every access.
unsafe impl<T: Send> Sync for RtMutex<T> {}

/// Unlocks on drop. Not `Send`: an unfair lock must be released by its owner thread.
pub struct RtGuard<'a, T> {
    mutex: &'a RtMutex<T>,
    #[cfg(not(target_os = "macos"))]
    _held: std::sync::MutexGuard<'a, ()>,
    _owner_thread: PhantomData<*const ()>,
}

impl<T> RtMutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            #[cfg(target_os = "macos")]
            lock: UnsafeCell::new(libc::OS_UNFAIR_LOCK_INIT),
            #[cfg(not(target_os = "macos"))]
            lock: std::sync::Mutex::new(()),
            value: UnsafeCell::new(value),
        }
    }

    /// Not re-entrant: locking twice on one thread aborts (macOS) or deadlocks.
    pub fn lock(&self) -> RtGuard<'_, T> {
        #[cfg(target_os = "macos")]
        // SAFETY: the lock lives as long as `self` and is never moved while held
        // (the guard borrows `self`).
        unsafe {
            libc::os_unfair_lock_lock(self.lock.get());
        }
        RtGuard {
            mutex: self,
            #[cfg(not(target_os = "macos"))]
            _held: self.lock.lock().unwrap_or_else(|e| e.into_inner()),
            _owner_thread: PhantomData,
        }
    }
}

impl<T> Deref for RtGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard holds the lock.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for RtGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard holds the lock, exclusively.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for RtGuard<'_, T> {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        // SAFETY: this thread took the lock in `RtMutex::lock`.
        unsafe {
            libc::os_unfair_lock_unlock(self.mutex.lock.get());
        }
    }
}

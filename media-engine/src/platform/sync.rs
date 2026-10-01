//! A mutex, over the camera's pthread implementation.
//!
//! The engine has exactly one shared lock: whoever owns the daemon connection
//! writes to it under this mutex, so a record from the microphone thread cannot
//! interleave with one from the thread serving commands - the reference engine's
//! `client_lock`. It has to block rather than spin, because a write to a daemon
//! that has stopped reading blocks until the socket drains.
//!
//! Like the thread attributes, `pthread_mutex_t` is opaque and differs between
//! libcs, so it is over-sized here and only ever touched through libc.

use super::Error;
use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, size_of};
use core::ops::{Deref, DerefMut};

/// Storage for a `pthread_mutex_t`, larger than any libc's definition of it.
#[repr(C, align(8))]
struct Storage([u8; 64]);

const _: () = assert!(size_of::<Storage>() >= 48);

/// A mutual-exclusion lock around a value.
pub struct Mutex<T> {
    // A pthread mutex must keep its initialization address. UnsafeCell also
    // permits libc to mutate its state through shared references to this lock.
    storage: ManuallyDrop<Box<UnsafeCell<Storage>>>,
    value: UnsafeCell<T>,
}

// SAFETY: the mutex is what makes access to the value safe, and it is held for
// the whole of a guard's life.
unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Create an initialized mutex.
    ///
    /// Initializing can fail in principle (the libc reports `ENOMEM`), so the
    /// caller sees it rather than a lock that misbehaves later.
    pub fn new(value: T) -> Result<Self, Error> {
        let storage = Box::new(UnsafeCell::new(Storage([0; 64])));
        let mut attributes = Storage([0; 64]);
        let attributes = (&mut attributes as *mut Storage).cast();
        // SAFETY: aligned, oversized storage for the opaque native attributes.
        let status = unsafe { libc::pthread_mutexattr_init(attributes) };
        if status != 0 {
            return Err(Error::from_errno(status));
        }
        // SAFETY: attributes are initialized. Explicit NORMAL semantics make
        // reentrant locking deadlock rather than invoking DEFAULT's POSIX UB.
        let mut status =
            unsafe { libc::pthread_mutexattr_settype(attributes, libc::PTHREAD_MUTEX_NORMAL) };
        if status == 0 {
            // SAFETY: the allocation stays at this address until destruction,
            // and the attributes are initialized. Only construct Self after
            // success so failure never destroys an uninitialized native mutex.
            status = unsafe { libc::pthread_mutex_init(storage.get().cast(), attributes) };
        }
        // SAFETY: initialization succeeded, regardless of later operations.
        unsafe { libc::pthread_mutexattr_destroy(attributes) };
        if status != 0 {
            return Err(Error::from_errno(status));
        }
        Ok(Self {
            storage: ManuallyDrop::new(storage),
            value: UnsafeCell::new(value),
        })
    }

    /// Block until the lock is free, then hold it.
    pub fn lock(&self) -> Result<Guard<'_, T>, Error> {
        // SAFETY: construction initialized the native mutex at this address;
        // UnsafeCell permits libc's interior mutation while locking.
        let status = unsafe { libc::pthread_mutex_lock(self.storage()) };
        if status != 0 {
            return Err(Error::from_errno(status));
        }
        Ok(Guard {
            mutex: self,
            _not_send: PhantomData,
        })
    }

    fn storage(&self) -> *mut libc::pthread_mutex_t {
        self.storage.get().cast()
    }
}

impl<T> Drop for Mutex<T> {
    fn drop(&mut self) {
        // A safe caller can forget a guard, leaving the mutex locked. POSIX
        // forbids destroying that mutex, so leak its storage in that case,
        // just as std's pthread-backed mutex does.
        // SAFETY: this mutex is initialized at a stable address. Exclusive
        // ownership rules out other users except a forgotten guard.
        if unsafe { libc::pthread_mutex_trylock(self.storage()) } == 0 {
            // SAFETY: trylock acquired the mutex on this thread; unlock before
            // destroying it, then free its backing storage exactly once.
            unsafe {
                libc::pthread_mutex_unlock(self.storage());
                libc::pthread_mutex_destroy(self.storage());
                ManuallyDrop::drop(&mut self.storage);
            }
        }
    }
}

/// The lock while it is held; releasing it happens on the acquiring thread.
///
/// A pthread mutex must not be unlocked by another thread, so a guard is not
/// `Send`, even when its protected value is:
///
/// ```compile_fail
/// use c220_media_engine::platform::sync::Guard;
/// fn require_send<T: Send>() {}
/// require_send::<Guard<'static, u32>>();
/// ```
pub struct Guard<'a, T> {
    mutex: &'a Mutex<T>,
    _not_send: PhantomData<*mut ()>,
}

// SAFETY: a shared guard only exposes &T, and cannot unlock or mutate it.
// Requiring T: Sync prevents sharing interior-mutable non-Sync values.
unsafe impl<T: Sync> Sync for Guard<'_, T> {}

impl<T> Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard only exists while the mutex is held.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and the guard is exclusive.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: this non-Send guard holds the lock on the acquiring thread,
        // and the borrowed mutex keeps its native storage alive.
        unsafe { libc::pthread_mutex_unlock(self.mutex.storage()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn serializes_two_threads() {
        // Without the lock this counter would lose increments; with it, every
        // one of them lands.
        let mutex = Arc::new(Mutex::new(0_u32).expect("mutex should initialize"));
        let increments = 5_000;
        let racers: Vec<_> = (0..4)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                std::thread::spawn(move || {
                    for _ in 0..increments {
                        let mut value = mutex.lock().expect("lock should succeed");
                        *value += 1;
                    }
                })
            })
            .collect();
        for racer in racers {
            racer.join().expect("thread should finish");
        }

        assert_eq!(*mutex.lock().expect("lock should succeed"), 4 * increments);
    }

    #[test]
    fn dropping_a_mutex_with_a_forgotten_guard_is_safe() {
        let mutex = Mutex::new(0_u32).expect("mutex should initialize");
        core::mem::forget(mutex.lock().expect("lock should succeed"));
        drop(mutex);
    }

    #[test]
    fn a_guard_releases_on_drop() {
        let mutex = Mutex::new(AtomicU32::new(0)).expect("mutex should initialize");
        {
            mutex
                .lock()
                .expect("lock should succeed")
                .fetch_add(1, Ordering::SeqCst);
        }
        // The lock is free again, so this cannot deadlock.
        assert_eq!(
            mutex
                .lock()
                .expect("lock should succeed")
                .load(Ordering::SeqCst),
            1
        );
    }
}

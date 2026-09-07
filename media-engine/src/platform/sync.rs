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
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ops::{Deref, DerefMut};

/// Storage for a `pthread_mutex_t`, larger than any libc's definition of it.
#[repr(C, align(8))]
struct Storage([u8; 64]);

const _: () = assert!(size_of::<Storage>() >= 48);

/// A mutual-exclusion lock around a value.
pub struct Mutex<T> {
    storage: Storage,
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
        let mutex = Self {
            storage: Storage([0; 64]),
            value: UnsafeCell::new(value),
        };
        // As with the thread calls, the error code is the return value: pthreads
        // do not report through `errno`.
        let status = unsafe { libc::pthread_mutex_init(mutex.storage(), core::ptr::null()) };
        if status != 0 {
            return Err(Error::from_errno(status));
        }
        Ok(mutex)
    }

    /// Block until the lock is free, then hold it.
    pub fn lock(&self) -> Result<Guard<'_, T>, Error> {
        let status = unsafe { libc::pthread_mutex_lock(self.storage()) };
        if status != 0 {
            return Err(Error::from_errno(status));
        }
        Ok(Guard { mutex: self })
    }

    fn storage(&self) -> *mut libc::pthread_mutex_t {
        &self.storage as *const Storage as *mut libc::pthread_mutex_t
    }
}

impl<T> Drop for Mutex<T> {
    fn drop(&mut self) {
        unsafe { libc::pthread_mutex_destroy(self.storage()) };
    }
}

/// The lock while it is held; releasing it happens on drop.
pub struct Guard<'a, T> {
    mutex: &'a Mutex<T>,
}

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

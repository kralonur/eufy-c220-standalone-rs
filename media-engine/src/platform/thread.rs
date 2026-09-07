//! Threads, over the camera's pthread implementation.
//!
//! The engine runs one producer thread per device that has to be read
//! continuously - the microphone now, the video path next - while the main
//! thread serves the daemon. That is the reference engine's shape too, and the
//! stack size it gives those threads is 1 MiB, which this keeps: an 8 KiB
//! default would be a stack overflow in a formatting path rather than a clean
//! failure.
//!
//! `pthread_attr_t` is opaque and its size is a libc detail that differs between
//! the musl bindings this crate is compiled against and the uClibc it runs on, so
//! it is over-sized here and only ever touched through libc.

use super::Error;
use alloc::boxed::Box;
use core::ffi::c_void;
use core::mem::size_of;

/// Stack size for the engine's producer threads, matching the reference engine.
const THREAD_STACK_SIZE: usize = 1024 * 1024;

/// Storage for a `pthread_attr_t`.
///
/// Larger than any libc's definition of it, which is all that matters: libc
/// reads and writes it through its own functions and never sees this type.
#[repr(C, align(8))]
struct ThreadAttributes([u8; 128]);

const _: () = assert!(size_of::<ThreadAttributes>() >= 64);

/// A spawned thread.
pub struct Thread {
    handle: libc::pthread_t,
    joined: bool,
}

impl Thread {
    /// Wait for the thread to finish.
    pub fn join(&mut self) {
        if !self.joined {
            self.joined = true;
            unsafe { libc::pthread_join(self.handle, core::ptr::null_mut()) };
        }
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        // A thread nobody joins must not keep its resources: detaching lets the
        // runtime reclaim it when it exits, which is what a stop request leads to.
        if !self.joined {
            unsafe { libc::pthread_detach(self.handle) };
        }
    }
}

/// Spawn `body` on its own thread.
pub fn spawn<F>(name: &'static str, body: F) -> Result<Thread, Error>
where
    F: FnOnce() + Send + 'static,
{
    let mut attributes = ThreadAttributes([0; 128]);
    let attributes = &mut attributes as *mut ThreadAttributes as *mut libc::pthread_attr_t;
    // pthreads return their error code and leave `errno` alone, so it is the
    // return value that goes into the error, not whatever `errno` happens to hold.
    let status = unsafe { libc::pthread_attr_init(attributes) };
    if status != 0 {
        return Err(Error::from_errno(status));
    }
    let status = unsafe { libc::pthread_attr_setstacksize(attributes, THREAD_STACK_SIZE) };
    if status != 0 {
        unsafe { libc::pthread_attr_destroy(attributes) };
        return Err(Error::from_errno(status));
    }

    // The closure travels to the trampoline as a raw pointer and is reclaimed
    // there, so it is freed exactly once whether the thread ran or not. It is
    // boxed twice on purpose: a `dyn` closure is a fat pointer, and a fat pointer
    // cannot survive the trip through `pthread_create`'s `void *`.
    let body: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(body));
    let argument = Box::into_raw(body) as *mut c_void;
    // `pthread_t` is an integer on some libcs and a pointer on others, so the
    // handle is uninitialized storage rather than a typed zero.
    let mut handle = core::mem::MaybeUninit::<libc::pthread_t>::uninit();
    let created =
        unsafe { libc::pthread_create(handle.as_mut_ptr(), attributes, trampoline, argument) };
    unsafe { libc::pthread_attr_destroy(attributes) };
    if created != 0 {
        drop(unsafe { Box::from_raw(argument as *mut Box<dyn FnOnce() + Send>) });
        return Err(Error::from_errno(created));
    }

    // SAFETY: a successful `pthread_create` wrote the handle.
    let handle = unsafe { handle.assume_init() };

    // A name makes the thread recognizable in `/proc`, which is the only
    // debugging interface the camera has; failing to set one is not an error.
    let mut name_buffer = [0_u8; 16];
    let taken = name.len().min(name_buffer.len() - 1);
    name_buffer[..taken].copy_from_slice(&name.as_bytes()[..taken]);
    unsafe { libc::pthread_setname_np(handle, name_buffer.as_ptr() as *const libc::c_char) };

    Ok(Thread {
        handle,
        joined: false,
    })
}

extern "C" fn trampoline(argument: *mut c_void) -> *mut c_void {
    // SAFETY: `spawn` hands over a boxed closure and nothing else calls this.
    let body = unsafe { Box::from_raw(argument as *mut Box<dyn FnOnce() + Send>) };
    body();
    core::ptr::null_mut()
}

/// Wait for the current thread, for the microseconds given.
pub fn sleep_us(microseconds: u32) {
    super::sleep_us(microseconds);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn runs_a_closure_and_joins_it() {
        let counter = Arc::new(AtomicU32::new(0));
        let mut thread = spawn("test-thread", {
            let counter = Arc::clone(&counter);
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })
        .expect("thread should spawn");

        thread.join();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_dropped_thread_does_not_leak_its_handle() {
        // Dropping without joining detaches; the assertion is that nothing
        // panicked and the work still happened.
        let counter = Arc::new(AtomicU32::new(0));
        let thread = spawn("test-detached", {
            let counter = Arc::clone(&counter);
            move || {
                sleep_us(1_000);
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })
        .expect("thread should spawn");
        drop(thread);

        for _ in 0..1_000 {
            if counter.load(Ordering::SeqCst) == 1 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the detached thread should still have run");
    }
}

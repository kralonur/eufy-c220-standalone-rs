//! Process shutdown: the flag a signal sets and the sockets it wakes.
//!
//! The engine is a supervisor-managed process, so it has to stop promptly on
//! `SIGTERM` without leaving a socket file or a blocked read behind. A signal
//! handler cannot do much, so it does exactly two things: set the stop flag and
//! shut the registered sockets down, which wakes the blocked `accept` or `read`
//! in the engine thread. File descriptors are closed by their owners, never by
//! the handler, so a descriptor cannot be closed twice or reused underneath a
//! thread that is still using it.

use super::Error;
use core::ffi::c_int;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};

/// The stop signal the daemon uses for a graceful shutdown.
const SIGTERM: c_int = libc::SIGTERM;

/// The interactive interrupt signal, treated the same way.
const SIGINT: c_int = libc::SIGINT;

/// Shared between the engine thread and the signal handler.
pub struct Shutdown {
    stop: AtomicBool,
    listener_fd: AtomicI32,
    client_fd: AtomicI32,
}

impl Shutdown {
    /// Create a shutdown state that no signal is wired to yet.
    pub const fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            listener_fd: AtomicI32::new(-1),
            client_fd: AtomicI32::new(-1),
        }
    }

    /// Whether a stop has been requested.
    pub fn requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Ask the engine to stop and wake anything blocked on the registered
    /// sockets.
    ///
    /// Only atomics and `shutdown(2)` happen here, so this is safe to call from
    /// a signal handler.
    pub fn request(&self) {
        self.stop.store(true, Ordering::SeqCst);
        for descriptor in [
            self.listener_fd.load(Ordering::SeqCst),
            self.client_fd.load(Ordering::SeqCst),
        ] {
            if descriptor >= 0 {
                unsafe { libc::shutdown(descriptor, libc::SHUT_RDWR) };
            }
        }
    }

    /// Remember the listening socket so a stop can wake `accept`.
    pub fn register_listener(&self, descriptor: c_int) {
        self.listener_fd.store(descriptor, Ordering::SeqCst);
    }

    /// Remember the connected socket so a stop can wake `read`.
    pub fn register_client(&self, descriptor: c_int) {
        self.client_fd.store(descriptor, Ordering::SeqCst);
    }

    /// Forget the connected socket after a session ended.
    pub fn clear_client(&self) {
        self.client_fd.store(-1, Ordering::SeqCst);
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// Install the engine's signal disposition.
///
/// `signal` is used rather than `sigaction` on purpose: `struct sigaction`
/// differs between the musl bindings this crate is compiled against and the
/// uClibc it runs on, while `signal`'s prototype is an integer and a handler.
/// `SIGPIPE` is ignored so that writing to a daemon that has already gone away
/// returns `EPIPE` instead of killing the engine.
///
/// `shutdown` must outlive the process, which it does: the caller keeps it for
/// the whole run.
pub fn install(shutdown: &Shutdown) -> Result<(), Error> {
    HANDLER_STATE.store(
        shutdown as *const Shutdown as *mut Shutdown,
        Ordering::SeqCst,
    );
    install_handler(SIGTERM)?;
    install_handler(SIGINT)?;
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    Ok(())
}

/// The state the handler reaches, or null before [`install`].
static HANDLER_STATE: AtomicPtr<Shutdown> = AtomicPtr::new(core::ptr::null_mut());

extern "C" fn handle_stop(_signal: c_int) {
    let state = HANDLER_STATE.load(Ordering::SeqCst);
    if !state.is_null() {
        // SAFETY: the pointer was stored by `install` from a `&Shutdown` the
        // caller keeps alive for the lifetime of the process, and `request` only
        // touches atomics and calls `shutdown(2)`.
        unsafe { (*state).request() };
    }
}

fn install_handler(signal: c_int) -> Result<(), Error> {
    let previous = unsafe { libc::signal(signal, handle_stop as *const () as libc::sighandler_t) };
    if previous == libc::SIG_ERR {
        return Err(Error::last());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::net::UnixListener;
    use std::os::unix::net::UnixStream as StdStream;
    use std::time::Duration;

    fn socket_path(name: &str) -> alloc::string::String {
        alloc::format!(
            "/tmp/c220-media-engine-{}-{}.sock",
            std::process::id(),
            name
        )
    }

    #[test]
    fn stops_are_visible_before_and_after_a_request() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.requested());
        shutdown.request();
        assert!(shutdown.requested());
    }

    #[test]
    fn shutting_down_wakes_a_blocked_read() {
        let path = socket_path("shutdown");
        let listener = UnixListener::bind(&path).expect("listener should bind");
        let client = std::thread::spawn({
            let path = path.clone();
            move || StdStream::connect(&path)
        });
        let accepted = listener.accept().expect("accept should succeed");
        // The peer stays connected and silent, so the read has nothing to return
        // until the shutdown wakes it.
        let peer = client
            .join()
            .expect("client thread")
            .expect("client should connect");

        let shutdown = Shutdown::new();
        shutdown.register_client(accepted.raw_fd());
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 1];
            let outcome = accepted.read(&mut buffer);
            let _ = sender.send(());
            outcome
        });

        // Give the reader a moment to block, then stop the engine. The channel
        // turns a regression into a failed test instead of a hung one.
        std::thread::sleep(Duration::from_millis(50));
        shutdown.request();
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("a shut-down socket must wake a blocked read");

        let outcome = reader.join().expect("reader thread");
        assert!(matches!(outcome, Ok(0) | Err(_)), "{outcome:?}");
        drop(peer);
        std::fs::remove_file(&path).expect("socket file should be removable");
    }
}

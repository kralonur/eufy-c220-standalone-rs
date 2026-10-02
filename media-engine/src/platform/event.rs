//! Durable eventfd wakeups for the private media writer. The board's uClibc
//! exports eventfd; its ABI is integers and an eight-byte kernel counter only.
use super::Error;

pub(crate) struct Event(libc::c_int);

impl Event {
    pub fn new() -> Result<Self, Error> {
        // SAFETY: eventfd has no pointer arguments; successful return owns one fd.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(Error::last());
        }
        Ok(Self(fd))
    }

    pub fn notify(&self) -> Result<(), Error> {
        // Linux eventfd adds this counter; any positive value wakes the reader.
        let counter = 1_u64;
        loop {
            // SAFETY: the fd remains owned and the counter is a live eight-byte buffer.
            let result = unsafe {
                libc::write(
                    self.0,
                    (&counter as *const u64).cast(),
                    core::mem::size_of::<u64>(),
                )
            };
            if result < 0 {
                let error = Error::last();
                if error.is_interrupted() {
                    continue;
                }
                // A saturated counter already contains a durable wakeup.
                if !error.would_block() {
                    return Err(error);
                }
            }
            return Ok(());
        }
    }

    pub fn wait(&self) -> Result<(), Error> {
        loop {
            let mut descriptor = libc::pollfd {
                fd: self.0,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives one live descriptor; -1 waits indefinitely.
            let status = unsafe { libc::poll(&mut descriptor, 1, -1) };
            if status < 0 {
                let error = Error::last();
                if error.is_interrupted() {
                    continue;
                }
                return Err(error);
            }
            let mut counter = 0_u64;
            // SAFETY: read writes at most the live counter's eight bytes.
            let read = unsafe {
                libc::read(
                    self.0,
                    (&mut counter as *mut u64).cast(),
                    core::mem::size_of::<u64>(),
                )
            };
            if read < 0 {
                let error = Error::last();
                if error.is_interrupted() || error.would_block() {
                    continue;
                }
                return Err(error);
            }
            return Ok(());
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: this value exclusively owns the descriptor.
        unsafe {
            libc::close(self.0);
        }
    }
}

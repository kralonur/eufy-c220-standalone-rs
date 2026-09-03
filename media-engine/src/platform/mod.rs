//! Thin, explicit wrappers over the camera's libc.
//!
//! The engine cannot use Rust's `std` (see the crate documentation), so the few
//! POSIX calls it needs are wrapped here by hand. Two rules keep this safe on
//! the board's uClibc runtime:
//!
//! * Structures passed across the libc boundary are defined *here* to the
//!   kernel's layout where the `libc` crate's definition follows musl and would
//!   differ, notably `struct timeval` (musl's 32-bit targets use a 64-bit
//!   `time_t`) and `struct sigaction` (which is why the engine uses `signal`).
//! * Nothing else is passed but integers and pointers.

use core::ffi::c_int;
use core::fmt;

pub mod alloc;
pub mod net;
pub mod signal;

/// A failed libc call, carrying the `errno` value that caused it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Error {
    errno: c_int,
}

impl Error {
    /// Capture the current `errno`.
    pub(crate) fn last() -> Self {
        Self {
            errno: unsafe { *libc::__errno_location() },
        }
    }

    /// Build an error for a known `errno` value.
    pub const fn from_errno(errno: c_int) -> Self {
        Self { errno }
    }

    /// The `errno` value.
    pub const fn errno(self) -> c_int {
        self.errno
    }

    /// The libc message for this `errno`, for diagnostics.
    pub fn message(self) -> &'static str {
        // `strerror` returns a pointer to a static buffer; it is not reentrant,
        // which is fine because this is only read to format a log line.
        unsafe {
            let text = libc::strerror(self.errno);
            if text.is_null() {
                return "unknown error";
            }
            core::ffi::CStr::from_ptr(text)
                .to_str()
                .unwrap_or("unknown error")
        }
    }

    /// Whether the call was interrupted by a signal.
    pub(crate) const fn is_interrupted(self) -> bool {
        self.errno == libc::EINTR
    }

    /// Whether a non-blocking or timed-out call would have blocked.
    ///
    /// `EWOULDBLOCK` is the same value as `EAGAIN` on Linux.
    pub(crate) const fn would_block(self) -> bool {
        self.errno == libc::EAGAIN
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "errno {}: {}", self.errno, self.message())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} (errno {})", self.message(), self.errno)
    }
}

impl core::error::Error for Error {}

/// Longest diagnostic line kept as one write.
///
/// Assembling the line first matters: several threads log, and a line split
/// across writes could interleave with another thread's and become unreadable.
/// Longer lines are truncated rather than dropped.
const LOG_LINE_LEN: usize = 256;

/// Write one diagnostic line to stderr, best effort.
///
/// Lines keep the reference engine's `media_key=value` shape, which the boot
/// console and the operator's shell are read by hand.
pub fn log(arguments: fmt::Arguments<'_>) {
    struct Line {
        bytes: [u8; LOG_LINE_LEN],
        len: usize,
    }

    impl fmt::Write for Line {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            let room = LOG_LINE_LEN - self.len;
            let taken = text.len().min(room);
            self.bytes[self.len..self.len + taken].copy_from_slice(&text.as_bytes()[..taken]);
            self.len += taken;
            // Truncating is not an error: the line is a diagnostic.
            Ok(())
        }
    }

    let mut line = Line {
        bytes: [0_u8; LOG_LINE_LEN],
        len: 0,
    };
    let _ = fmt::Write::write_fmt(&mut line, arguments);
    let _ = fmt::Write::write_str(&mut line, "\n");
    let mut remaining = &line.bytes[..line.len.min(LOG_LINE_LEN)];
    while !remaining.is_empty() {
        let written = unsafe {
            libc::write(
                libc::STDERR_FILENO,
                remaining.as_ptr() as *const core::ffi::c_void,
                remaining.len(),
            )
        };
        if written <= 0 {
            return;
        }
        remaining = &remaining[written as usize..];
    }
}

/// Log one diagnostic line: `media_log!("media_key={}", value)`.
macro_rules! media_log {
    ($($argument:tt)*) => {
        $crate::platform::log(format_args!($($argument)*))
    };
}

pub(crate) use media_log;

/// Sleep for the given number of microseconds.
///
/// `usleep` takes an unsigned integer, so no structure crosses the boundary and
/// the reference engine's sleeps map over directly.
pub fn sleep_us(microseconds: u32) {
    unsafe { libc::usleep(microseconds) };
}

/// Terminate immediately, without running destructors.
pub fn exit(code: c_int) -> ! {
    unsafe { libc::_exit(code) }
}

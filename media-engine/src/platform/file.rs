//! Small file helpers for the sysfs interfaces the engine drives.
//!
//! GPIO is files in `/sys/class/gpio`, so the engine needs plain open/write and
//! an existence check; there is no need for a general filesystem layer.

use super::Error;
use alloc::ffi::CString;
use core::ffi::c_void;

/// Whether `path` exists.
pub fn exists(path: &str) -> bool {
    let Ok(path) = CString::new(path) else {
        return false;
    };
    unsafe { libc::access(path.as_ptr(), libc::F_OK) == 0 }
}

/// Write `text` to `path`, creating it when the kernel allows it.
pub fn write_text(path: &str, text: &str) -> Result<(), Error> {
    let path = CString::new(path).map_err(|_| Error::from_errno(libc::EINVAL))?;
    let descriptor = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if descriptor < 0 {
        return Err(Error::last());
    }

    let mut remaining = text.as_bytes();
    let mut result = Ok(());
    while !remaining.is_empty() {
        let written = unsafe {
            libc::write(
                descriptor,
                remaining.as_ptr() as *const c_void,
                remaining.len(),
            )
        };
        if written < 0 {
            let error = Error::last();
            if error.is_interrupted() {
                continue;
            }
            result = Err(error);
            break;
        }
        if written == 0 {
            result = Err(Error::from_errno(libc::EIO));
            break;
        }
        remaining = &remaining[written as usize..];
    }

    unsafe { libc::close(descriptor) };
    result
}

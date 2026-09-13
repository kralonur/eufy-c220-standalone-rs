//! Bindings to the camera's vendor libraries.
//!
//! These exist only for the camera target: they declare the Rockchip MPI, AIQ
//! and FDK AAC interfaces the engine drives, and they cannot be linked anywhere
//! else. Everything here follows the legacy C engine, which declared these
//! structures by hand: the layouts and their size assertions are ported with
//! them, because a wrong layout here is a memory corruption bug on the camera
//! rather than a compile error.
//!
//! Three conventions are worth knowing before reading on:
//!
//! * **Everything returns an `i32` status** where `0` is success, which
//!   [`check`] turns into a `Result`.
//! * **Buffers are vendor-owned.** Memory the encoder or the audio devices fill
//!   comes from the Rockchip memory allocator, so it is wrapped in RAII types
//!   ([`MmzBuffer`], [`VencStreamBuffer`]) instead of being handed around as
//!   raw pointers.
//! * **AIQ symbols are versioned** in `librkaiq.so` (the default version is
//!   `CODEABI_3.8`), which is why the reference engine's references resolve
//!   against a plain name and the declarations here do too.

use crate::platform::Error;

pub mod aiq;
pub mod fdk;
pub mod mpi;

/// The vendor status code that means success.
pub const RK_SUCCESS: i32 = 0;

/// Turn a vendor status code into a `Result`.
pub fn check(status: i32) -> Result<(), Error> {
    if status == RK_SUCCESS {
        Ok(())
    } else {
        Err(Error::from_vendor_status(status))
    }
}

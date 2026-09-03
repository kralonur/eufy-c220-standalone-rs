//! The engine's allocator: one `mmap` per allocation.
//!
//! The engine allocates its record buffers as it starts and then allocates
//! nothing in the steady state - the media buffers themselves come from the
//! vendor memory allocator - so the simple scheme is a page-rounded anonymous
//! mapping per allocation, with the mapping length and the payload offset stored
//! immediately in front of the payload.
//!
//! This lives in the crate rather than in the entry module so it can be tested
//! directly: on the host the tests call it through the [`GlobalAlloc`] interface
//! while `std`'s allocator stays installed.

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_int, c_void};
use core::mem::size_of;
use core::ptr;

// Anonymous mapping over the target's `mmap`.
//
// The camera's uClibc declares `off_t` as 32-bit, so its `mmap` takes a
// different argument layout than the `libc` crate's musl bindings describe and
// the kernel answers `EINVAL`; `mmap64` is the variant whose 64-bit offset
// matches the bindings. On a 64-bit host both are the same call, which is what
// keeps this testable off the camera.
#[cfg(target_pointer_width = "32")]
unsafe extern "C" {
    #[link_name = "mmap64"]
    fn platform_mmap(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        descriptor: c_int,
        offset: i64,
    ) -> *mut c_void;
}

// Anonymous mapping over the target's `mmap`, for hosts whose `off_t` is the
// 64-bit type the bindings already use.
#[cfg(target_pointer_width = "64")]
unsafe extern "C" {
    #[link_name = "mmap"]
    fn platform_mmap(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        descriptor: c_int,
        offset: i64,
    ) -> *mut c_void;
}

/// Bytes reserved in front of every payload for the two `usize` fields below.
///
/// Sixteen bytes keep the payload 16-byte aligned, which is what the mapping's
/// own alignment already provides for the layouts this crate uses.
const HEADER_LEN: usize = 2 * size_of::<usize>();

/// The engine's global allocator.
pub struct MmapAllocator;

impl MmapAllocator {
    /// Allocate `layout`, returning null when the mapping fails.
    ///
    /// # Safety
    ///
    /// The caller must treat the returned pointer as a [`GlobalAlloc`]
    /// allocation: it is only valid until [`GlobalAlloc::dealloc`] is called with
    /// the same layout.
    unsafe fn map(layout: Layout) -> *mut u8 {
        // Reserve room for the header plus the worst-case alignment padding, then
        // align the payload inside the mapping.
        let Some(reserve) = HEADER_LEN.checked_add(layout.align()) else {
            return ptr::null_mut();
        };
        let Some(total) = reserve.checked_add(layout.size()) else {
            return ptr::null_mut();
        };

        let mapping = unsafe {
            platform_mmap(
                ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if mapping == libc::MAP_FAILED {
            // The engine has no other way to report a failing mapping, and an
            // allocation failure is fatal: say why before the panic handler
            // prints a message with no errno in it.
            let error = super::Error::last();
            crate::platform::media_log!(
                "media_allocation_error={} size={}",
                error.errno(),
                layout.size()
            );
            return ptr::null_mut();
        }

        let base = mapping as usize;
        let payload = (base + HEADER_LEN + layout.align() - 1) & !(layout.align() - 1);
        let offset = payload - base;
        // The header sits directly in front of the payload so `dealloc` can find
        // it from the payload pointer alone.
        let header = (payload - HEADER_LEN) as *mut usize;
        unsafe {
            header.write(total);
            header.add(1).write(offset);
        }
        payload as *mut u8
    }
}

unsafe impl GlobalAlloc for MmapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            // `GlobalAlloc::alloc` requires a non-zero size; a zero-sized request
            // is handed a dangling, well-aligned pointer instead.
            return layout.align() as *mut u8;
        }
        unsafe { Self::map(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if pointer.is_null() || layout.size() == 0 {
            return;
        }
        let header = unsafe { pointer.sub(HEADER_LEN) as *const usize };
        let total = unsafe { header.read() };
        let offset = unsafe { header.add(1).read() };
        let base = unsafe { pointer.sub(offset) };
        unsafe { libc::munmap(base as *mut c_void, total) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Allocate through the allocator directly, because `std` owns the global
    /// allocator in a test binary.
    fn allocate(layout: Layout) -> *mut u8 {
        unsafe { GlobalAlloc::alloc(&MmapAllocator, layout) }
    }

    fn free(pointer: *mut u8, layout: Layout) {
        unsafe { GlobalAlloc::dealloc(&MmapAllocator, pointer, layout) }
    }

    #[test]
    fn honours_alignment_and_keeps_the_payload_writable() {
        for align in [1, 2, 4, 8, 16, 64, 4096] {
            let layout = Layout::from_size_align(1000, align).expect("layout");
            let pointer = allocate(layout);
            assert!(!pointer.is_null(), "align {align}");
            assert_eq!(pointer as usize % align, 0, "align {align}");
            unsafe {
                for index in 0..layout.size() {
                    pointer.add(index).write(index as u8);
                }
                for index in 0..layout.size() {
                    assert_eq!(pointer.add(index).read(), index as u8, "align {align}");
                }
            }
            free(pointer, layout);
        }
    }

    #[test]
    fn backs_a_rust_vector_and_survives_growth() {
        // A large enough vector that it cannot come from any small-object cache,
        // exercising mapping and unmapping repeatedly through `alloc`/`dealloc`.
        let mut values: Vec<u64> = Vec::new();
        for value in 0..20_000_u64 {
            values.push(value);
        }
        assert_eq!(values.iter().sum::<u64>(), 199_990_000);
        let peak = values.capacity();
        values.shrink_to_fit();
        assert!(values.capacity() <= peak);
        assert_eq!(values[19_999], 19_999);
    }

    #[test]
    fn zero_sized_requests_get_a_dangling_pointer() {
        let layout = Layout::from_size_align(0, 8).expect("layout");
        let pointer = allocate(layout);
        assert_eq!(pointer as usize, 8);
        free(pointer, layout);
    }
}

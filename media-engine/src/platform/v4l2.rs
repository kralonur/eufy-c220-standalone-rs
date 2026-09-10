//! Video for Linux 2: the kernel interface the capture path is built on.
//!
//! The declarations are written here rather than taken from a crate.
//! `linux-raw-sys` is `no_std`-friendly but has no V4L2 types at all, and the
//! crates that do - `v4l`, `v4l2r` - generate their bindings with `bindgen` at
//! build time, which needs kernel headers and libclang for a uClibc cross build
//! and would put a `std`-shaped dependency in a `no_std` binary.
//!
//! # These layouts are the camera's 32-bit ABI
//!
//! A V4L2 request number encodes `sizeof(struct)` in its own bits, and
//! `struct v4l2_buffer` contains an `unsigned long`, so the numbers and the
//! layouts differ between a 32-bit camera and a 64-bit development host. Both
//! were read out of the same headers and the same `arm-linux-musleabihf` target
//! the reference engine compiles with, by compiling a program that prints them
//! (the values in the comments below), and the size assertions hold this module
//! to that ABI.
//!
//! The measured facts the code relies on:
//!
//! | Structure | Size | Fields used |
//! | --- | --- | --- |
//! | `v4l2_capability` | 104 | `capabilities` at 84, `device_caps` at 88 |
//! | `v4l2_format` | 204 | `type` at 0, `pix_mp` at 4 |
//! | `v4l2_pix_format_mplane` | 192 | `num_planes` at 180, `plane_fmt` at 20 |
//! | `v4l2_requestbuffers` | 20 | `count` at 0, `type` at 4, `memory` at 8 |
//! | `v4l2_buffer` | 80 | `m` at 64, `length` at 68, `sequence` at 56 |
//! | `v4l2_plane` | 60 | `bytesused` at 0, `length` at 4, `m` at 8 |
//! | `v4l2_streamparm` | 204 | `parm.capture.timeperframe` at 12 |
//!
//! `struct timeval` inside `v4l2_buffer` is two 32-bit `long`s on the camera, so
//! the timestamp is declared with `c_long` fields and stays correct on either
//! target.

use super::Error;
use alloc::ffi::CString;
use core::ffi::{c_int, c_long, c_short, c_ulong, c_void};
use core::mem::{self, size_of};

/* --- request numbers, as the 32-bit camera ABI encodes them --- */

/// `_IOR('V', 0, struct v4l2_capability)`, 104 bytes: `0x80685600`.
pub const QUERYCAP: c_ulong = 0x8068_5600;

/// `_IOWR('V', 4, struct v4l2_format)`, 204 bytes: `0xc0cc5604`.
pub const G_FMT: c_ulong = 0xc0cc_5604;

/// `_IOWR('V', 5, struct v4l2_format)`: `0xc0cc5605`.
pub const S_FMT: c_ulong = 0xc0cc_5605;

/// `_IOWR('V', 8, struct v4l2_requestbuffers)`, 20 bytes: `0xc0145608`.
pub const REQBUFS: c_ulong = 0xc014_5608;

/// `_IOWR('V', 15, struct v4l2_buffer)`, 80 bytes: `0xc050560f`.
pub const QBUF: c_ulong = 0xc050_560f;

/// `_IOWR('V', 17, struct v4l2_buffer)`: `0xc0505611`.
pub const DQBUF: c_ulong = 0xc050_5611;

/// `_IOW('V', 18, enum v4l2_buf_type)`, 4 bytes: `0x40045612`.
pub const STREAMON: c_ulong = 0x4004_5612;

/// `_IOW('V', 19, enum v4l2_buf_type)`: `0x40045613`.
pub const STREAMOFF: c_ulong = 0x4004_5613;

/// `_IOWR('V', 21, struct v4l2_streamparm)`: `0xc0cc5615`.
pub const G_PARM: c_ulong = 0xc0cc_5615;

/// `_IOWR('V', 22, struct v4l2_streamparm)`: `0xc0cc5616`.
pub const S_PARM: c_ulong = 0xc0cc_5616;

/* --- values --- */

/// Multi-planar video capture, which is what the ISP node is.
pub const BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;

/// The engine supplies its own buffer, so the driver must not allocate one.
pub const MEMORY_USERPTR: u32 = 2;

/// The device can capture in the multi-planar API.
pub const CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x0000_1000;

/// The device supports streaming I/O.
pub const CAP_STREAMING: u32 = 0x0400_0000;

/// `device_caps` in the capability result is valid.
pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

/// NV12: one luma plane followed by interleaved chroma, which is what the ISP
/// and the encoder both want.
pub const PIX_FMT_NV12: u32 = 0x3231_564e;

/// Plane slots the kernel's structures reserve.
pub const VIDEO_MAX_PLANES: usize = 8;

/// The driver can report and accept a frame interval.
pub const CAP_TIMEPERFRAME: u32 = 0x0000_1000;

/// `V4L2_FIELD_...`: the 2K sensor mode is progressive, and the encoder is told
/// the same value when the frame is handed over.
pub const FIELD_NONE: u32 = 1;

/// `struct timeval` as `struct v4l2_buffer` embeds it.
///
/// The camera's libc is musl, whose `time_t` is 64-bit on every architecture and
/// whose `suseconds_t` is a `long`: the structure is 16 bytes with 8-byte
/// alignment there, which is what puts the timestamp at offset 24 of the buffer
/// after four bytes of padding. A 32-bit `time_t` layout would put it at 20 and
/// make the whole request a different size - and a different ioctl number.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeval {
    pub seconds: i64,
    pub microseconds: c_long,
}

/// `struct v4l2_capability`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

impl core::fmt::Debug for Capability {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Capability")
            .field("version", &self.version)
            .field("capabilities", &format_args!("{:#010x}", self.capabilities))
            .field("device_caps", &format_args!("{:#010x}", self.device_caps))
            .finish()
    }
}

impl Capability {
    /// The capabilities of this device rather than of the driver.
    ///
    /// A device that reports `V4L2_CAP_DEVICE_CAPS` means `device_caps`; older
    /// ones only fill `capabilities`, which is what the reference engine checks
    /// as well.
    pub const fn effective_capabilities(&self) -> u32 {
        if self.capabilities & CAP_DEVICE_CAPS != 0 {
            self.device_caps
        } else {
            self.capabilities
        }
    }
}

/// One plane's format description.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlanePixFormat {
    pub size_image: u32,
    pub bytes_per_line: u32,
    pub reserved: [u32; 3],
}

/// `struct v4l2_pix_format_mplane`: the multi-planar format.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PixFormatMplane {
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [PlanePixFormat; VIDEO_MAX_PLANES],
    pub num_planes: u8,
    pub flags: u8,
    pub ycbcr_enc: u8,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

/// The format union of `struct v4l2_format`.
///
/// Only the multi-planar member is used; the raw member fixes the size the
/// kernel expects, because the union also holds the overlay formats.
#[repr(C)]
#[derive(Clone, Copy)]
pub union FormatUnion {
    pub pix_mp: PixFormatMplane,
    pub raw: [u8; 200],
}

/// `struct v4l2_format`: the type tag plus its union.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Format {
    pub buffer_type: u32,
    pub fmt: FormatUnion,
}

impl Format {
    /// An all-zero format for the given buffer type.
    pub fn new(buffer_type: u32) -> Self {
        Self {
            buffer_type,
            fmt: FormatUnion { raw: [0; 200] },
        }
    }

    /// The multi-planar format.
    ///
    /// # Safety
    ///
    /// The union is only meaningful once the driver has filled in a multi-planar
    /// format, which is what [`G_FMT`] on a capture queue does.
    pub unsafe fn pix_mp(&self) -> PixFormatMplane {
        unsafe { self.fmt.pix_mp }
    }

    /// The multi-planar format, mutably.
    ///
    /// # Safety
    ///
    /// As [`Format::pix_mp`].
    pub unsafe fn pix_mp_mut(&mut self) -> &mut PixFormatMplane {
        unsafe { &mut self.fmt.pix_mp }
    }
}

/// `struct v4l2_plane`: one plane of one buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Plane {
    pub bytes_used: u32,
    pub length: u32,
    pub memory: PlaneMemory,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

/// How a plane's memory was supplied.
#[repr(C)]
#[derive(Clone, Copy)]
pub union PlaneMemory {
    pub user_pointer: c_ulong,
    pub shared_fd: i32,
}

/// `struct v4l2_buffer`: one queued or dequeued buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Buffer {
    pub index: u32,
    pub buffer_type: u32,
    pub bytes_used: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: Timeval,
    pub timecode: [u8; 16],
    pub sequence: u32,
    pub memory: u32,
    pub m: BufferMemory,
    pub length: u32,
    pub reserved2: u32,
    /// A union of `__s32 request_fd` and `__u32 reserved` in the kernel header.
    pub request_fd: i32,
}

/// Where a buffer's memory is, depending on the memory type.
#[repr(C)]
#[derive(Clone, Copy)]
pub union BufferMemory {
    pub offset: u32,
    pub user_pointer: c_ulong,
    pub planes: *mut Plane,
    pub descriptor: i32,
}

impl Buffer {
    /// A buffer description for one multi-planar USERPTR buffer.
    pub fn user_pointer(buffer_type: u32, planes: *mut Plane) -> Self {
        Self {
            index: 0,
            buffer_type,
            bytes_used: 0,
            flags: 0,
            field: 0,
            timestamp: Timeval::default(),
            timecode: [0; 16],
            sequence: 0,
            memory: MEMORY_USERPTR,
            m: BufferMemory { planes },
            length: 1,
            reserved2: 0,
            request_fd: 0,
        }
    }
}

/// `struct v4l2_requestbuffers`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RequestBuffers {
    pub count: u32,
    pub buffer_type: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub reserved: [u32; 1],
}

impl RequestBuffers {
    /// Ask for `count` caller-supplied capture buffers.
    pub fn user_pointer(count: u32) -> Self {
        Self {
            count,
            buffer_type: BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            memory: MEMORY_USERPTR,
            capabilities: 0,
            reserved: [0],
        }
    }
}

/// A frame interval as a fraction of a second.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fract {
    pub numerator: u32,
    pub denominator: u32,
}

/// `struct v4l2_captureparm`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CaptureParm {
    pub capability: u32,
    pub capture_mode: u32,
    pub time_per_frame: Fract,
    pub extended_mode: u32,
    pub read_buffers: u32,
    pub reserved: [u32; 4],
}

/// The parameter union of `struct v4l2_streamparm`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union StreamParmUnion {
    pub capture: CaptureParm,
    pub raw: [u8; 200],
}

/// `struct v4l2_streamparm`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StreamParm {
    pub buffer_type: u32,
    pub parm: StreamParmUnion,
}

impl StreamParm {
    /// A parameter block for the capture queue.
    pub fn capture() -> Self {
        Self {
            buffer_type: BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            parm: StreamParmUnion { raw: [0; 200] },
        }
    }

    /// The capture parameters.
    ///
    /// # Safety
    ///
    /// The union is only meaningful for a capture queue, which is what this type
    /// is built for.
    pub unsafe fn capture_parm(&self) -> CaptureParm {
        unsafe { self.parm.capture }
    }

    /// The capture parameters, mutably.
    ///
    /// # Safety
    ///
    /// As [`StreamParm::capture_parm`].
    pub unsafe fn capture_parm_mut(&mut self) -> &mut CaptureParm {
        unsafe { &mut self.parm.capture }
    }
}

/* --- layout assertions ---
 *
 * The ones that hold on any target are unconditional, so a host build checks
 * them too; the two structures holding an `unsigned long` are held to the
 * camera's 32-bit layout only there.
 */

const _: () = assert!(size_of::<Capability>() == 104);
const _: () = assert!(size_of::<PixFormatMplane>() == 192);
const _: () = assert!(size_of::<PlanePixFormat>() == 20);
const _: () = assert!(size_of::<Format>() == 204);
const _: () = assert!(size_of::<RequestBuffers>() == 20);
const _: () = assert!(size_of::<StreamParm>() == 204);
const _: () = assert!(size_of::<CaptureParm>() == 40);
const _: () = assert!(size_of::<Fract>() == 8);

#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<Buffer>() == 80);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<Plane>() == 60);
const _: () = assert!(size_of::<Timeval>() == 16);

/// A capture device and its queue.
pub struct CaptureDevice {
    descriptor: c_int,
    streaming: bool,
}

impl CaptureDevice {
    /// Open a capture node.
    ///
    /// The node is opened non-blocking, so a frame that has not arrived yet is a
    /// poll away rather than a blocked read - the reference engine opens it the
    /// same way.
    pub fn open(path: &str) -> Result<Self, Error> {
        let path = CString::new(path).map_err(|_| Error::from_errno(libc::EINVAL))?;
        let descriptor = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(Error::last());
        }
        Ok(Self {
            descriptor,
            streaming: false,
        })
    }

    /// Ask the driver what the device can do.
    pub fn capabilities(&self) -> Result<Capability, Error> {
        let mut capability: Capability = unsafe { mem::zeroed() };
        self.request(QUERYCAP, &mut capability as *mut Capability as *mut c_void)?;
        Ok(capability)
    }

    /// Ask the driver for the current capture format.
    pub fn format(&self) -> Result<Format, Error> {
        let mut format = Format::new(BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        self.request(G_FMT, &mut format as *mut Format as *mut c_void)?;
        Ok(format)
    }

    /// Tell the driver how many caller-supplied buffers to expect.
    ///
    /// The driver may adjust the count, which the caller has to check: the
    /// reference engine requires at least what it asked for.
    pub fn request_user_buffers(&self, count: u32) -> Result<u32, Error> {
        let mut buffers = RequestBuffers::user_pointer(count);
        self.request(REQBUFS, &mut buffers as *mut RequestBuffers as *mut c_void)?;
        Ok(buffers.count)
    }

    /// Hand a queued buffer back to the driver, ready for capture.
    pub fn queue(&self, index: u32, pointer: *mut u8, length: usize) -> Result<(), Error> {
        let mut planes = [Plane {
            bytes_used: 0,
            length: length as u32,
            memory: PlaneMemory {
                user_pointer: pointer as c_ulong,
            },
            data_offset: 0,
            reserved: [0; 11],
        }; VIDEO_MAX_PLANES];
        let mut buffer = Buffer::user_pointer(BUF_TYPE_VIDEO_CAPTURE_MPLANE, planes.as_mut_ptr());
        buffer.index = index;
        self.request(QBUF, &mut buffer as *mut Buffer as *mut c_void)
    }

    /// Take the next filled buffer from the driver.
    ///
    /// A non-blocking dequeue with nothing ready reports
    /// [`Error::would_block`](super::Error::would_block), which the capture loop
    /// treats as "go round again".
    pub fn dequeue(&self) -> Result<Dequeued, Error> {
        let mut planes = [Plane {
            bytes_used: 0,
            length: 0,
            memory: PlaneMemory { user_pointer: 0 },
            data_offset: 0,
            reserved: [0; 11],
        }; VIDEO_MAX_PLANES];
        let mut buffer = Buffer::user_pointer(BUF_TYPE_VIDEO_CAPTURE_MPLANE, planes.as_mut_ptr());
        self.request(DQBUF, &mut buffer as *mut Buffer as *mut c_void)?;
        Ok(Dequeued {
            index: buffer.index,
            timestamp: buffer.timestamp,
            sequence: buffer.sequence,
            flags: buffer.flags,
            bytes_used: planes[0].bytes_used,
        })
    }

    /// Start capturing.
    pub fn start_streaming(&mut self) -> Result<(), Error> {
        let mut buffer_type = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        self.request(STREAMON, &mut buffer_type as *mut u32 as *mut c_void)?;
        self.streaming = true;
        Ok(())
    }

    /// Stop capturing.
    pub fn stop_streaming(&mut self) -> Result<(), Error> {
        let mut buffer_type = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        self.request(STREAMOFF, &mut buffer_type as *mut u32 as *mut c_void)?;
        self.streaming = false;
        Ok(())
    }

    /// The frame interval the driver is currently delivering.
    pub fn frame_interval(&self) -> Result<Fract, Error> {
        let mut parm = StreamParm::capture();
        self.request(G_PARM, &mut parm as *mut StreamParm as *mut c_void)?;
        // SAFETY: this type is always built for the capture queue.
        Ok(unsafe { parm.capture_parm() }.time_per_frame)
    }

    /// Ask the driver for a frame interval of `1/fps`.
    ///
    /// The driver reports back what it settled on, which is not necessarily what
    /// was asked for - the sensor mode decides that.
    pub fn set_frame_interval(&self, fps: u32) -> Result<Fract, Error> {
        let mut parm = StreamParm::capture();
        // SAFETY: as above.
        unsafe { parm.capture_parm_mut() }.time_per_frame = Fract {
            numerator: 1,
            denominator: fps,
        };
        self.request(S_PARM, &mut parm as *mut StreamParm as *mut c_void)?;
        // SAFETY: as above.
        Ok(unsafe { parm.capture_parm() }.time_per_frame)
    }

    /// The capability flags that describe the frame interval API.
    pub fn frame_interval_capability(&self) -> Result<u32, Error> {
        let mut parm = StreamParm::capture();
        self.request(G_PARM, &mut parm as *mut StreamParm as *mut c_void)?;
        // SAFETY: as above.
        Ok(unsafe { parm.capture_parm() }.capability)
    }

    /// Wait for the device to have something to read, up to `timeout_ms`.
    ///
    /// `Ok(false)` is a timeout, which is how the capture loop notices that the
    /// device has stopped producing frames.
    pub fn wait_readable(&self, timeout_ms: i32) -> Result<bool, Error> {
        let mut descriptor = libc::pollfd {
            fd: self.descriptor,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if ready >= 0 {
                return Ok(ready > 0);
            }
            let error = Error::last();
            if !error.is_interrupted() {
                return Err(error);
            }
        }
    }

    /// The raw descriptor, for diagnostics.
    pub fn raw_descriptor(&self) -> c_int {
        self.descriptor
    }

    /// Issue one `ioctl`, retrying an interrupted call.
    ///
    /// Every V4L2 call goes through here, which is the reference engine's
    /// `xioctl`.
    fn request(&self, request: c_ulong, argument: *mut c_void) -> Result<(), Error> {
        loop {
            let result = unsafe { libc::ioctl(self.descriptor, request as _, argument) };
            if result >= 0 {
                return Ok(());
            }
            let error = Error::last();
            if !error.is_interrupted() {
                return Err(error);
            }
        }
    }
}

impl Drop for CaptureDevice {
    fn drop(&mut self) {
        if self.streaming {
            // Best effort: the driver is about to lose this descriptor anyway.
            let mut buffer_type = BUF_TYPE_VIDEO_CAPTURE_MPLANE;
            unsafe {
                libc::ioctl(
                    self.descriptor,
                    STREAMOFF as _,
                    &mut buffer_type as *mut u32 as *mut c_void,
                )
            };
        }
        // Releasing the buffers before closing is what the reference engine does;
        // it also tells the driver to drop its reference to the caller's memory,
        // which is about to be freed.
        let mut zero = RequestBuffers::user_pointer(0);
        unsafe {
            libc::ioctl(
                self.descriptor,
                REQBUFS as _,
                &mut zero as *mut RequestBuffers as *mut c_void,
            )
        };
        unsafe { libc::close(self.descriptor) };
    }
}

impl core::fmt::Debug for CaptureDevice {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CaptureDevice")
            .field("descriptor", &self.descriptor)
            .field("streaming", &self.streaming)
            .finish()
    }
}

/// One frame the driver handed back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dequeued {
    /// Which buffer it is, so it can be queued again.
    pub index: u32,
    /// When the sensor produced it.
    pub timestamp: Timeval,
    /// The driver's frame counter.
    pub sequence: u32,
    /// Buffer flags, including the error flag.
    pub flags: u32,
    /// Bytes the driver wrote into the first plane.
    pub bytes_used: u32,
}

impl Dequeued {
    /// The timestamp in microseconds, or `None` when the driver did not provide
    /// one.
    ///
    /// This is the reference engine's `capture_pts_us`: a timestamp is used only
    /// when both halves are non-negative and not both zero.
    pub const fn timestamp_us(&self) -> Option<u64> {
        let seconds = self.timestamp.seconds;
        let microseconds = self.timestamp.microseconds;
        if seconds < 0 || microseconds < 0 || (seconds == 0 && microseconds == 0) {
            return None;
        }
        Some(seconds as u64 * 1_000_000 + microseconds as u64)
    }
}

/// Ignore a raw descriptor value in a `c_short` field, keeping the compiler
/// quiet about the sign of `poll`'s event masks.
const _: () = assert!(size_of::<c_short>() == 2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_device_is_an_error() {
        let error = CaptureDevice::open("/dev/definitely-not-a-capture-node")
            .expect_err("opening a missing node must fail");
        assert_eq!(error.errno(), Some(libc::ENOENT), "{error:?}");
    }

    #[test]
    fn a_device_without_v4l2_reports_the_kernel_error() {
        // `/dev/null` opens fine and is not a V4L2 device, so the request itself
        // has to fail cleanly rather than be assumed to work.
        let device = CaptureDevice::open("/dev/null").expect("/dev/null should open");
        let error = device
            .capabilities()
            .expect_err("QUERYCAP on /dev/null must fail");
        assert_eq!(error.errno(), Some(libc::ENOTTY), "{error:?}");
        assert!(!device.streaming);
    }

    #[test]
    fn captures_a_timestamp_only_when_the_driver_gave_one() {
        let mut dequeued = Dequeued {
            index: 0,
            timestamp: Timeval {
                seconds: 12,
                microseconds: 345,
            },
            sequence: 0,
            flags: 0,
            bytes_used: 0,
        };
        assert_eq!(dequeued.timestamp_us(), Some(12_000_345));

        dequeued.timestamp = Timeval::default();
        assert_eq!(dequeued.timestamp_us(), None);

        dequeued.timestamp = Timeval {
            seconds: -1,
            microseconds: -1,
        };
        assert_eq!(dequeued.timestamp_us(), None);
    }

    #[test]
    fn a_format_union_is_the_size_the_kernel_expects() {
        let mut format = Format::new(BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        // SAFETY: a capture queue's union member is the multi-planar format.
        let pix_mp = unsafe { format.pix_mp_mut() };
        pix_mp.width = 2304;
        pix_mp.height = 1296;
        pix_mp.pixel_format = PIX_FMT_NV12;
        pix_mp.num_planes = 1;
        // SAFETY: as above.
        let read_back = unsafe { format.pix_mp() };
        assert_eq!(read_back.width, 2304);
        assert_eq!(read_back.height, 1296);
        assert_eq!(read_back.pixel_format, PIX_FMT_NV12);
        assert_eq!(read_back.num_planes, 1);
    }

    #[test]
    fn effective_capabilities_choose_the_device_view() {
        let mut capability = Capability {
            driver: [0; 16],
            card: [0; 32],
            bus_info: [0; 32],
            version: 0,
            capabilities: CAP_STREAMING,
            device_caps: 0,
            reserved: [0; 3],
        };
        // Without the flag, what the driver reports is what the device is.
        assert_eq!(capability.effective_capabilities(), CAP_STREAMING);

        // With it, only `device_caps` counts.
        capability.capabilities = CAP_DEVICE_CAPS | CAP_STREAMING;
        capability.device_caps = CAP_VIDEO_CAPTURE_MPLANE;
        assert_eq!(
            capability.effective_capabilities(),
            CAP_VIDEO_CAPTURE_MPLANE
        );
    }
}

//! The video path: V4L2 capture, the vendor encoder, and the records it produces.
//!
//! Ported from the reference engine's `open_video`, `video_thread` and
//! `close_video`, minus the detection sub-stream that was dropped. The shape is
//! fixed by the hardware: one caller-supplied buffer is queued to the capture
//! node, each frame that arrives is handed to the encoder in place, and the
//! encoded access unit the encoder returns is what becomes a record.
//!
//! Three details are worth knowing before reading this:
//!
//! * **The frame rate is the rate the sensor really delivers** (about 15.17 fps,
//!   measured), not what the capture node is asked for. The encoder's constant
//!   bitrate mode divides its budget by the declared frame rate, so declaring a
//!   rate the hardware never reaches silently starves every frame of bits.
//! * **Cache maintenance is not optional.** The capture buffer is written by the
//!   ISP and the encoded packs are written by the encoder, both by DMA past the
//!   CPU cache, so the buffer is written back before handing it to the encoder
//!   and the encoder's packs are invalidated before being read.
//! * **The encoder reports no keyframe flag.** The reference engine decides one
//!   frame at a time in software, by scanning the access unit for an IRAP NAL
//!   type, which is what [`crate::keyframe::detect`] ports.
//!
//! The image path (`start_aiq`, the ISP tuning) is not part of this yet: capture
//! and encoding do not depend on it, but the picture stays untuned until it
//! lands, and the reference engine starts it right before the buffers are queued.

use crate::client::RecordSink;
use crate::platform::signal::Shutdown;
use crate::platform::thread::{self, Thread};
use crate::platform::v4l2::{self, CaptureDevice};
use crate::platform::{Error, media_log, sleep_us};
use crate::pts::Counter;
use crate::vendor::mpi::{
    CbrAttributes, ChannelAttributes, EncodedStreamBuffer, EncoderAttributes, EncoderChannel,
    GopAttributes, MmzBuffer, RateControlAttributes, VideoFrame, VideoFrameInfo,
};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use c220_media_protocol::RecordKind;

/// Capture width the sensor's 2K mode provides.
pub const WIDTH: u32 = 2304;

/// Capture height the sensor's 2K mode provides.
pub const HEIGHT: u32 = 1296;

/// Bytes in one NV12 frame.
const FRAME_SIZE: usize = (WIDTH as usize) * (HEIGHT as usize) * 3 / 2;

/// The reference engine queues one buffer, which is all the encoder needs.
const INPUT_BUFFERS: u32 = 1;

/// Largest access unit assembled from the encoder's packs (reference value).
const MAX_ACCESS_UNIT: usize = 1024 * 1024;

/// The encoder channel the reference engine uses for the main stream.
const VENC_CHANNEL: i32 = 1;

/// Pack slots the stream description is built with (reference value).
const MAX_PACKS: usize = 8;

/// `RK_FMT_YUV420SP`: NV12, the format the ISP produces.
const PIXEL_FORMAT_YUV420SP: i32 = 0;

/// `RK_TRUE`: the encoder is handed whole frames rather than fields.
const BY_FRAME: i32 = 1;

/// Encoder field value the reference engine passes.
///
/// The vendor's field enum is not V4L2's: this is its "frame" value, which the
/// reference engine uses for a progressive 2K capture.
const ENCODER_FIELD: i32 = 4;

/// Codec byte for video records in the record ABI.
const CODEC_H265: u8 = 2;

/// Encoder buffer pool size (reference value).
const ENCODER_BUFFER_SIZE: u32 = 1024 * 1024;

/// Stream buffer count the channel keeps (reference value).
const ENCODER_STREAM_BUFFERS: u32 = 2;

/// Timeout for handing a frame to the encoder, in milliseconds (reference value).
const SEND_TIMEOUT_MS: i32 = 2000;

/// Timeout for collecting the encoded output, in milliseconds (reference value).
const STREAM_TIMEOUT_MS: i32 = 3000;

/// How long to wait for a frame before counting the wait as empty, in
/// milliseconds (reference value).
const POLL_TIMEOUT_MS: i32 = 3000;

/// Pause after a failed hand-off, in microseconds (reference value).
const SEND_RETRY_PAUSE_US: u32 = 2000;

/// Timestamp step for a frame the driver did not stamp, in microseconds: the
/// reference engine's 50 ms.
const FALLBACK_STEP_US: u64 = 50_000;

/// Frames between statistics lines (reference value).
const STATS_INTERVAL: u64 = 75;

/// Name the capture thread carries in `/proc`.
const VIDEO_THREAD: &str = "c220-video";

/// Where the video path captures and how its encoder is configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoConfig {
    /// Capture node, as the daemon named it.
    pub device: String,
    /// Frames per second the capture path delivers.
    pub fps: u32,
    /// Constant-bitrate budget in kilobits per second.
    pub bitrate_kbps: u32,
}

impl VideoConfig {
    /// The engine's defaults, matching the reference engine's.
    pub fn new(device: &str, fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            device: String::from(device),
            fps,
            bitrate_kbps,
        }
    }
}

/// The encoder attributes the reference engine creates its channel with.
fn encoder_attributes(fps: u32, bitrate_kbps: u32) -> ChannelAttributes {
    ChannelAttributes {
        encoder: EncoderAttributes {
            codec: crate::vendor::mpi::VIDEO_ID_HEVC,
            max_picture_width: WIDTH,
            max_picture_height: HEIGHT,
            pixel_format: PIXEL_FORMAT_YUV420SP,
            mirror: 0,
            buffer_size: ENCODER_BUFFER_SIZE,
            profile: crate::vendor::mpi::H265_PROFILE_MAIN,
            by_frame: BY_FRAME,
            picture_width: WIDTH,
            picture_height: HEIGHT,
            virtual_width: WIDTH,
            virtual_height: HEIGHT,
            stream_buffer_count: ENCODER_STREAM_BUFFERS,
            reserved: [0; 7],
        },
        rate_control: RateControlAttributes {
            mode: crate::vendor::mpi::RC_MODE_H265_CBR,
            h265_cbr: CbrAttributes {
                // Two seconds of pictures per group, as in the reference engine.
                gop: fps * 2,
                source_frame_rate_num: fps,
                source_frame_rate_den: 1,
                destination_frame_rate_num: fps,
                destination_frame_rate_den: 1,
                bitrate_kbps,
                statistics_seconds: 1,
                reserved: [0; 2],
            },
        },
        gop: GopAttributes {
            mode: crate::vendor::mpi::GOP_MODE_NORMAL_P,
            reserved: [0; 3],
        },
    }
}

/// The open capture and encode path, before its thread starts.
struct Pipeline {
    device: CaptureDevice,
    input: MmzBuffer,
    channel: EncoderChannel,
}

/// Everything the capture path takes from the media heap before the ISP starts.
///
/// The reference engine does all of this in `main` and before `start_aiq`: open
/// the capture node, allocate the input frame, create the encoder channel and tell
/// it to receive.
///
/// The order is not cosmetic. Once AIQ is running and holding its own buffers, the
/// heap has nothing left for a full frame, so the allocation fails; creating the
/// encoder channel at that point takes the process down with it, which is what the
/// camera showed: silence in the log, no signal, the vendor library exiting.
/// Preparing first is what makes the difference.
pub struct VideoPrepared {
    device: CaptureDevice,
    input: MmzBuffer,
    channel: EncoderChannel,
}

/// Open the capture node, take the input frame and create the encoder channel.
pub fn prepare(config: &VideoConfig) -> Result<VideoPrepared, Error> {
    let device = CaptureDevice::open(&config.device).inspect_err(|error| {
        media_log!("media_video_open_error={error}");
    })?;

    let capability = device.capabilities().inspect_err(|error| {
        media_log!("media_video_querycap_error={error}");
    })?;
    let caps = capability.effective_capabilities();
    if caps & v4l2::CAP_VIDEO_CAPTURE_MPLANE == 0 || caps & v4l2::CAP_STREAMING == 0 {
        media_log!("media_video_caps=0x{caps:08x}");
        return Err(Error::from_errno(libc::EINVAL));
    }

    let format = device.format().inspect_err(|error| {
        media_log!("media_video_g_fmt_error={error}");
    })?;
    // SAFETY: the capture queue's format member is the multi-planar one.
    let pix_mp = unsafe { format.pix_mp() };
    if pix_mp.width != WIDTH
        || pix_mp.height != HEIGHT
        || pix_mp.pixel_format != v4l2::PIX_FMT_NV12
        || pix_mp.num_planes != 1
    {
        media_log!("media_video_format_unsupported=1");
        return Err(Error::from_errno(libc::EINVAL));
    }

    let input = MmzBuffer::allocate(FRAME_SIZE).inspect_err(|error| {
        media_log!("media_video_mmz_error={error}");
    })?;

    let channel = EncoderChannel::create(
        VENC_CHANNEL,
        &encoder_attributes(config.fps, config.bitrate_kbps),
    )
    .inspect_err(|error| {
        media_log!("media_venc_error={error}");
    })?;

    Ok(VideoPrepared {
        device,
        input,
        channel,
    })
}

/// The camera's video path: the capture thread and everything it owns.
pub struct VideoPipeline {
    thread: Thread,
}

impl VideoPipeline {
    /// Open the capture node and the encoder, then start capturing.
    ///
    /// The devices are opened here rather than inside the thread so a camera
    /// whose sensor or encoder is missing says so at startup, and they are moved
    /// into the thread so that they are released by the thread that used them -
    /// the order the reference engine releases them in.
    pub fn open(
        config: &VideoConfig,
        prepared: VideoPrepared,
        sink: Arc<RecordSink>,
        shutdown: Arc<Shutdown>,
    ) -> Result<Self, Error> {
        let pipeline = open_pipeline(prepared, config.fps)?;

        media_log!(
            "media_video_ready=h265 {}x{} fps={} bitrate={}",
            WIDTH,
            HEIGHT,
            config.fps,
            config.bitrate_kbps
        );

        // The thread body is `'static`, so what it needs is taken by value.
        let fps = config.fps;

        // Declared in release order: the last value declared is dropped first, so
        // the capture stops before the encoder is torn down, as in the reference
        // engine's `close_video`.
        let mut channel = pipeline.channel;
        let mut input = pipeline.input;
        let mut device = pipeline.device;
        let thread = thread::spawn(VIDEO_THREAD, move || {
            capture_loop(&mut device, &mut input, &mut channel, fps, &sink, &shutdown)
        })?;
        Ok(Self { thread })
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        // The thread stops because shutdown was requested; joining is what makes
        // the capture node and the encoder certainly released before the engine
        // returns.
        self.thread.join();
    }
}

/// Queue the capture buffer, start streaming and hand the devices to the thread.
///
/// This runs after the ISP is up: the reference engine starts AIQ between
/// preparing and queueing, because the buffers it queues are the ones the ISP
/// fills.
fn open_pipeline(prepared: VideoPrepared, fps: u32) -> Result<Pipeline, Error> {
    let VideoPrepared {
        mut device,
        mut input,
        channel,
    } = prepared;

    let count = device
        .request_user_buffers(INPUT_BUFFERS)
        .inspect_err(|error| {
            media_log!("media_video_reqbufs_error={error}");
        })?;
    if count < INPUT_BUFFERS {
        media_log!("media_video_reqbufs_error={count} buffers");
        return Err(Error::from_errno(libc::ENOMEM));
    }
    device
        .queue(0, input.as_mut_slice().as_mut_ptr(), FRAME_SIZE)
        .inspect_err(|error| {
            media_log!("media_video_qbuf_error={error}");
        })?;
    device.start_streaming().inspect_err(|error| {
        media_log!("media_video_streamon_error={error}");
    })?;

    // Report the interval the sensor mode really provides, then ask for the
    // configured rate: a mismatch is what starves frames of bits.
    if let Ok(interval) = device.frame_interval() {
        let capability = device.frame_interval_capability().unwrap_or(0);
        media_log!(
            "media_capture_interval={}/{} caps=0x{:08x}",
            interval.numerator,
            interval.denominator,
            capability
        );
    }
    match device.set_frame_interval(fps) {
        Ok(interval) => media_log!(
            "media_capture_requested={} result={}/{}",
            fps,
            interval.numerator,
            interval.denominator
        ),
        Err(error) => media_log!("media_capture_request_failed={error}"),
    }

    Ok(Pipeline {
        device,
        input,
        channel,
    })
}

/// Capture, encode and send frames until asked to stop.
fn capture_loop(
    device: &mut CaptureDevice,
    input: &mut MmzBuffer,
    channel: &mut EncoderChannel,
    fps: u32,
    sink: &RecordSink,
    shutdown: &Shutdown,
) {
    let mut access_unit = vec![0_u8; MAX_ACCESS_UNIT];
    let mut stream = EncodedStreamBuffer::<MAX_PACKS>::new();
    let mut fallback = Counter::new(FALLBACK_STEP_US);
    let mut stats = Stats::default();

    while !shutdown.requested() {
        if !matches!(device.wait_readable(POLL_TIMEOUT_MS), Ok(true)) {
            stats.empty += 1;
            continue;
        }

        let frame = match device.dequeue() {
            Ok(frame) => frame,
            Err(_) => {
                // A frame that could not be taken is not worth a log line: the
                // device is polled again immediately.
                stats.empty += 1;
                continue;
            }
        };

        if (frame.bytes_used as usize) < FRAME_SIZE {
            stats.short += 1;
            requeue(device, input, frame.index);
            continue;
        }

        // The ISP wrote the frame past the CPU cache, so write it back before the
        // encoder reads it.
        input.flush();

        let pts_us = frame.timestamp_us().unwrap_or_else(|| fallback.advance());
        stats.last_pts_us = pts_us;
        let base = input.as_mut_slice().as_mut_ptr();
        let info = VideoFrameInfo {
            frame: VideoFrame {
                block: input.block(),
                width: WIDTH,
                height: HEIGHT,
                virtual_width: WIDTH,
                virtual_height: HEIGHT,
                field: ENCODER_FIELD,
                pixel_format: PIXEL_FORMAT_YUV420SP,
                video_format: 0,
                compress_mode: 0,
                dynamic_range: 0,
                color_gamut: 0,
                virtual_addresses: [
                    base as *mut core::ffi::c_void,
                    unsafe { base.add((WIDTH as usize) * (HEIGHT as usize)) }
                        as *mut core::ffi::c_void,
                    core::ptr::null_mut(),
                ],
                pts: pts_us,
                time_ref: 0,
                reserved: 0,
            },
            pool_id: 0,
            reserved: 0,
        };

        if channel.send_frame(&info, SEND_TIMEOUT_MS).is_err() {
            stats.send_errors += 1;
            requeue(device, input, frame.index);
            sleep_us(SEND_RETRY_PAUSE_US);
            continue;
        }

        let collected = channel.get_stream(&mut stream, STREAM_TIMEOUT_MS);
        // The buffer goes back as soon as the encoder has taken its copy, whether
        // or not the output was ready: the reference engine requeues here too, so
        // a failing encoder cannot stall capture.
        requeue(device, input, frame.index);
        if collected.is_err() {
            stats.stream_errors += 1;
            continue;
        }

        let mut total = 0;
        for pack in stream.packs() {
            let length = pack.length as usize;
            if total + length > MAX_ACCESS_UNIT {
                break;
            }
            // Reading an encoded pack is reading DMA output: invalidate before.
            let Some(data) = pack.data() else { continue };
            access_unit[total..total + data.len()].copy_from_slice(data);
            total += data.len();
        }
        if channel.release_stream(&mut stream).is_err() {
            stats.stream_errors += 1;
        }

        if total > 0 {
            stats.captured += 1;
            stats.bytes += total as u64;
            if stats.first_pts_us == 0 {
                stats.first_pts_us = pts_us;
            }
            let keyframe = crate::keyframe::detect(&access_unit[..total]);
            // A failed send means the daemon went away; the session thread
            // notices that too and detaches the sink, so this is not worth a log
            // line per frame, exactly as in the reference engine.
            let _ = sink.send(
                RecordKind::VIDEO,
                CODEC_H265,
                keyframe,
                pts_us,
                &access_unit[..total],
            );
        }

        stats.iterations += 1;
        if stats.iterations % STATS_INTERVAL == 0 {
            media_log!(
                "media_capture_stats captured={} empty={} short={} send_err={} stream_err={} bytes={} fps={} pts_first_us={} pts_last_us={}",
                stats.captured,
                stats.empty,
                stats.short,
                stats.send_errors,
                stats.stream_errors,
                stats.bytes,
                fps,
                stats.first_pts_us,
                stats.last_pts_us
            );
        }
    }
}

/// Give the capture buffer back to the driver.
fn requeue(device: &mut CaptureDevice, input: &mut MmzBuffer, index: u32) {
    if device
        .queue(index, input.as_mut_slice().as_mut_ptr(), FRAME_SIZE)
        .is_err()
    {
        // Nothing useful is left to do with a device that will not take its
        // buffer back; the next poll or dequeue reports the real failure.
        media_log!("media_video_qbuf_error=1");
    }
}

/// Counters for the periodic statistics line.
#[derive(Default)]
struct Stats {
    captured: u64,
    empty: u64,
    short: u64,
    send_errors: u64,
    stream_errors: u64,
    bytes: u64,
    first_pts_us: u64,
    last_pts_us: u64,
    iterations: u64,
}

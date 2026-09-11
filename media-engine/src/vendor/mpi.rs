//! Rockchip MPI: video encoder, audio capture/playback, and vendor memory.
//!
//! The structures here are the ones the reference engine declares by hand, in
//! the same field order and with the same reserved fields, and each size
//! assertion it carries is repeated below: a mismatch is silent memory
//! corruption on the camera, so it is worth failing the build for.

use super::{RK_SUCCESS, check};
use crate::platform::Error;
use core::ffi::{c_char, c_int, c_void};
use core::mem::size_of;
use core::ptr;

/// An opaque vendor memory block, the Rockchip equivalent of a DMA buffer.
pub type Block = *mut c_void;

/* --- status and identity constants from the reference engine --- */

/// The video encoder identity for HEVC, the codec the main stream uses.
pub const VIDEO_ID_HEVC: i32 = 12;

/// `YUV420SP`, which is NV12: the layout both the ISP and the encoder produce.
pub const FORMAT_YUV420SP: i32 = 0;

/// H.265 Main profile, the only profile the camera's encoder is asked for.
pub const H265_PROFILE_MAIN: u32 = 1;

/// Constant-bitrate rate control for H.265.
pub const RC_MODE_H265_CBR: i32 = 8;

/// Normal P-frame group-of-pictures; the reference engine uses nothing else.
pub const GOP_MODE_NORMAL_P: i32 = 1;

/// G.711 A-law, packed into one byte per sample.
pub const AUDIO_ID_PCM_ALAW: i32 = 0x4000_0002;

/// Microphone sample rate: 8 kHz, which is what the G.711 stream carries.
pub const AUDIO_SAMPLE_RATE_8000: i32 = 8000;

/// Speaker input sample rate: the talkback path resamples 16 kHz down to 8 kHz.
pub const AUDIO_SAMPLE_RATE_16000: i32 = 16000;

/// 16-bit samples.
pub const AUDIO_BIT_WIDTH_16: i32 = 1;

/// Single-channel audio for capture and playback.
pub const AUDIO_SOUND_MODE_MONO: i32 = 0;

/// Interleaved stereo, used for the speaker output track.
pub const AUDIO_SOUND_MODE_STEREO: i32 = 1;

/// Take the microphone from the front-left channel of the card.
pub const AUDIO_TRACK_FRONT_LEFT: i32 = 8;

/// Play out to the stereo pair.
pub const AUDIO_TRACK_OUT_STEREO: i32 = 10;

/// The default channel mode; the reference engine never asks for another.
pub const AUDIO_CHN_MODE_DEFAULT: i32 = 0;

/// No loopback: capture and playback stay independent.
pub const AUDIO_LOOPBACK_NONE: i32 = 0;

/// Module id of the audio input device.
pub const ID_AI: i32 = 10;

/// Module id of the audio encoder.
pub const ID_AENC: i32 = 12;

/// Room for the ALSA card name in [`AudioAttributes`], including the NUL.
pub const MAX_AUDIO_CARD_NAME: usize = 64;

/// Frames the encoder keeps in flight; the reference engine asks for two.
pub const VENC_STREAM_BUFFER_COUNT: u32 = 2;

/* --- video encoder --- */

/// Constant-bitrate parameters for H.265.
///
/// Field names follow the vendor headers so the structure can be compared with
/// the reference engine side by side; the reserved words are part of the ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CbrAttributes {
    pub gop: u32,
    pub source_frame_rate_num: u32,
    pub source_frame_rate_den: u32,
    pub destination_frame_rate_num: u32,
    pub destination_frame_rate_den: u32,
    pub bitrate_kbps: u32,
    pub statistics_seconds: u32,
    pub reserved: [u32; 2],
}

/// Rate control: the mode plus the mode's parameters.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RateControlAttributes {
    pub mode: i32,
    pub h265_cbr: CbrAttributes,
}

/// Group-of-pictures mode.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GopAttributes {
    pub mode: i32,
    pub reserved: [u32; 3],
}

/// Encoder channel attributes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EncoderAttributes {
    pub codec: i32,
    pub max_picture_width: u32,
    pub max_picture_height: u32,
    pub pixel_format: i32,
    pub mirror: i32,
    pub buffer_size: u32,
    pub profile: u32,
    pub by_frame: i32,
    pub picture_width: u32,
    pub picture_height: u32,
    pub virtual_width: u32,
    pub virtual_height: u32,
    pub stream_buffer_count: u32,
    pub reserved: [u32; 7],
}

/// Everything a VENC channel needs at creation time.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ChannelAttributes {
    pub encoder: EncoderAttributes,
    pub rate_control: RateControlAttributes,
    pub gop: GopAttributes,
}

/// How many frames a channel should encode; `-1` means "until told to stop".
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ReceivePictureParams {
    pub receive_picture_count: i32,
}

/// One captured frame as the encoder sees it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VideoFrame {
    pub block: Block,
    pub width: u32,
    pub height: u32,
    pub virtual_width: u32,
    pub virtual_height: u32,
    pub field: i32,
    pub pixel_format: i32,
    pub video_format: i32,
    pub compress_mode: i32,
    pub dynamic_range: i32,
    pub color_gamut: i32,
    pub virtual_addresses: [*mut c_void; 3],
    pub pts: u64,
    pub time_ref: u32,
    pub reserved: u32,
}

/// A captured frame plus the pool it came from.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VideoFrameInfo {
    pub frame: VideoFrame,
    pub pool_id: u32,
    pub reserved: u32,
}

/// One encoded chunk produced by the encoder.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EncodedPack {
    pub block: Block,
    pub length: u32,
    pub pts: u64,
    pub frame_end: i32,
    pub stream_end: i32,
    pub reserved: [u8; 112],
}

/// Encoded chunks for one frame, as handed back by the encoder.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EncodedStream {
    pub packs: *mut EncodedPack,
    pub pad: [u32; 3],
    pub pack_count: u32,
    pub sequence: u32,
    pub reserved: [u8; 376],
}

const _: () = assert!(size_of::<CbrAttributes>() == 36);
const _: () = assert!(size_of::<RateControlAttributes>() == 40);
const _: () = assert!(size_of::<GopAttributes>() == 16);
const _: () = assert!(size_of::<EncoderAttributes>() == 80);
const _: () = assert!(size_of::<ChannelAttributes>() == 0x88);
const _: () = assert!(size_of::<VideoFrame>() == 72);
const _: () = assert!(size_of::<VideoFrameInfo>() == 80);
const _: () = assert!(size_of::<EncodedPack>() == 0x88);
const _: () = assert!(size_of::<EncodedStream>() == 0x190);

/* --- audio --- */

/// The ALSA card the audio devices open.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SoundCard {
    pub channels: u32,
    pub sample_rate: u32,
    pub bit_width: i32,
}

/// Attributes shared by the audio input and output devices.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioAttributes {
    pub card: SoundCard,
    pub sample_rate: i32,
    pub bit_width: i32,
    pub sound_mode: i32,
    pub extended_flags: u32,
    pub frame_count: u32,
    pub points_per_frame: u32,
    pub channel_count: u32,
    pub card_name: [u8; MAX_AUDIO_CARD_NAME],
}

/// Capture channel parameters.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioInputChannelParams {
    pub user_frame_depth: i32,
    pub loopback_mode: i32,
}

/// Playback channel parameters.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioOutputChannelParams {
    pub mode: i32,
    pub loopback_mode: i32,
}

/// One encoded chunk of microphone audio.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioStream {
    pub block: Block,
    pub length: u32,
    pub timestamp: u64,
    pub sequence: u32,
    pub bypass_block: i32,
}

/// Codec parameters of an audio encoder channel.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioCodecAttributes {
    pub codec: i32,
    pub bit_width: i32,
    pub channels: u32,
    pub sample_rate: u32,
    pub bits_per_coded_sample: u32,
    pub bitrate: u32,
    pub reserved: [u32; 4],
    pub reserved_pointer: *mut c_void,
}

/// Everything an audio encoder channel needs at creation time.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioEncoderAttributes {
    pub codec: i32,
    pub buffer_count: u32,
    pub codec_attributes: AudioCodecAttributes,
    pub depth: u32,
}

/// One frame of PCM on its way to the speaker.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioFrame {
    pub block: Block,
    pub bit_width: i32,
    pub sound_mode: i32,
    pub timestamp: u64,
    pub sequence: u32,
    pub length: u32,
    pub bypass_block: i32,
}

/// Description of a caller-owned buffer the engine hands to a vendor device.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExternalBlockConfig {
    pub virtual_address: *mut u8,
    pub reserved0: u32,
    pub physical_address: u64,
    pub descriptor: i32,
    pub reserved1: u32,
    pub size: u64,
    pub free_callback: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    pub opaque: *mut c_void,
}

/// Identifies one channel of one device, used to bind capture to encoding.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Channel {
    pub module: i32,
    pub device: i32,
    pub channel: i32,
}

const _: () = assert!(size_of::<SoundCard>() == 12);
const _: () = assert!(size_of::<AudioAttributes>() == 104);
const _: () = assert!(size_of::<AudioStream>() == 24);
const _: () = assert!(size_of::<AudioCodecAttributes>() == 44);
const _: () = assert!(size_of::<AudioEncoderAttributes>() == 56);
const _: () = assert!(size_of::<AudioFrame>() == 40);
const _: () = assert!(size_of::<ExternalBlockConfig>() == 40);
const _: () = assert!(size_of::<Channel>() == 12);

unsafe extern "C" {
    pub fn RK_MPI_SYS_Init() -> i32;
    pub fn RK_MPI_SYS_Exit() -> i32;
    fn RK_MPI_SYS_MmzAlloc(
        block: *mut Block,
        name: *const c_char,
        option: *const c_char,
        size: u32,
    ) -> i32;
    fn RK_MPI_SYS_MmzFree(block: Block) -> i32;
    fn RK_MPI_SYS_MmzFlushCache(block: Block, is_read: i32) -> i32;
    fn RK_MPI_MB_Handle2VirAddr(block: Block) -> *mut c_void;
    fn RK_MPI_SYS_CreateMB(block: *mut Block, config: *const ExternalBlockConfig) -> i32;
    fn RK_MPI_MB_ReleaseMB(block: Block) -> i32;
    pub fn RK_MPI_SYS_Bind(source: *const Channel, destination: *const Channel) -> i32;
    pub fn RK_MPI_SYS_UnBind(source: *const Channel, destination: *const Channel) -> i32;

    /// The attributes are written through, so they must be writable.
    fn RK_MPI_VENC_CreateChn(channel: c_int, attributes: *mut ChannelAttributes) -> i32;
    fn RK_MPI_VENC_DestroyChn(channel: c_int) -> i32;
    /// The parameters are written through, so they must be writable.
    fn RK_MPI_VENC_StartRecvFrame(channel: c_int, params: *mut ReceivePictureParams) -> i32;
    fn RK_MPI_VENC_StopRecvFrame(channel: c_int) -> i32;
    /// The frame description is written through, so it must be writable.
    fn RK_MPI_VENC_SendFrame(channel: c_int, frame: *mut VideoFrameInfo, timeout_ms: i32) -> i32;
    fn RK_MPI_VENC_GetStream(channel: c_int, stream: *mut EncodedStream, timeout_ms: i32) -> i32;
    fn RK_MPI_VENC_ReleaseStream(channel: c_int, stream: *mut EncodedStream) -> i32;

    pub fn RK_MPI_AI_SetPubAttr(device: c_int, attributes: *const AudioAttributes) -> i32;
    pub fn RK_MPI_AI_Enable(device: c_int) -> i32;
    pub fn RK_MPI_AI_Disable(device: c_int) -> i32;
    /// The parameter structure is written through, so it must be writable.
    pub fn RK_MPI_AI_SetChnParam(
        device: c_int,
        channel: c_int,
        params: *mut AudioInputChannelParams,
    ) -> i32;
    pub fn RK_MPI_AI_EnableChn(device: c_int, channel: c_int) -> i32;
    pub fn RK_MPI_AI_DisableChn(device: c_int, channel: c_int) -> i32;
    pub fn RK_MPI_AI_SetTrackMode(device: c_int, mode: i32) -> i32;
    /// The attributes are written through, so they must be writable - see
    /// [`AudioEncoder::open`], which is where that was found.
    pub fn RK_MPI_AENC_CreateChn(channel: c_int, attributes: *mut AudioEncoderAttributes) -> i32;
    pub fn RK_MPI_AENC_DestroyChn(channel: c_int) -> i32;
    pub fn RK_MPI_AENC_GetStream(channel: c_int, stream: *mut AudioStream, timeout_ms: i32) -> i32;
    pub fn RK_MPI_AENC_ReleaseStream(channel: c_int, stream: *mut AudioStream) -> i32;

    pub fn RK_MPI_AO_SetPubAttr(device: c_int, attributes: *const AudioAttributes) -> i32;
    pub fn RK_MPI_AO_Enable(device: c_int) -> i32;
    pub fn RK_MPI_AO_Disable(device: c_int) -> i32;
    pub fn RK_MPI_AO_SetChnParams(
        device: c_int,
        channel: c_int,
        params: *const AudioOutputChannelParams,
    ) -> i32;
    pub fn RK_MPI_AO_EnableChn(device: c_int, channel: c_int) -> i32;
    pub fn RK_MPI_AO_EnableReSmp(device: c_int, channel: c_int, input_rate: i32) -> i32;
    pub fn RK_MPI_AO_DisableReSmp(device: c_int, channel: c_int) -> i32;
    pub fn RK_MPI_AO_DisableChn(device: c_int, channel: c_int) -> i32;
    pub fn RK_MPI_AO_ClearChnBuf(device: c_int, channel: c_int) -> i32;
    pub fn RK_MPI_AO_SetTrackMode(device: c_int, mode: i32) -> i32;
    pub fn RK_MPI_AO_SetVolume(device: c_int, volume: i32) -> i32;
    pub fn RK_MPI_AO_SendFrame(
        device: c_int,
        channel: c_int,
        frame: *const AudioFrame,
        timeout_ms: i32,
    ) -> i32;
}

/// A vendor memory block the engine owns.
///
/// The reference engine allocates these once at startup and frees them on the
/// way out; tying the block to a value means an early return cannot leak it.
pub struct MmzBuffer {
    block: Block,
    pointer: *mut u8,
    length: usize,
}

// SAFETY: the block is a vendor handle and the mapping behind it, and neither is
// bound to a thread: the vendor memory allocator is shared by the whole process,
// and moving the value is what keeps the block used by one thread at a time. The
// video path relies on this to hand its input buffer to the capture thread.
unsafe impl Send for MmzBuffer {}

impl MmzBuffer {
    /// Allocate `length` bytes of vendor memory.
    pub fn allocate(length: usize) -> Result<Self, Error> {
        let Ok(size) = u32::try_from(length) else {
            return Err(Error::from_errno(libc::EINVAL));
        };
        let mut block: Block = ptr::null_mut();
        check(unsafe { RK_MPI_SYS_MmzAlloc(&mut block, ptr::null(), ptr::null(), size) })?;
        if block.is_null() {
            return Err(Error::from_errno(libc::ENOMEM));
        }
        let pointer = unsafe { RK_MPI_MB_Handle2VirAddr(block) } as *mut u8;
        if pointer.is_null() {
            unsafe { RK_MPI_SYS_MmzFree(block) };
            return Err(Error::from_errno(libc::ENOMEM));
        }
        Ok(Self {
            block,
            pointer,
            length,
        })
    }

    /// The vendor block, for handing to a device call.
    pub fn block(&self) -> Block {
        self.block
    }

    /// The buffer length in bytes.
    pub fn len(&self) -> usize {
        self.length
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// The buffer as bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is valid for `length` bytes for as long as this
        // value lives, and `&self` keeps it alive.
        unsafe { core::slice::from_raw_parts(self.pointer, self.length) }
    }

    /// The buffer as writable bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes the exclusive borrow explicit.
        unsafe { core::slice::from_raw_parts_mut(self.pointer, self.length) }
    }

    /// Drop stale cache lines before reading what a device wrote.
    ///
    /// This is the reference engine's `MmzFlushCache(block, RK_TRUE)`: without
    /// it the CPU can read bytes the ISP or encoder wrote only into memory.
    pub fn invalidate(&self) {
        unsafe { RK_MPI_SYS_MmzFlushCache(self.block, 1) };
    }

    /// Write back cache lines before handing the buffer to a device.
    ///
    /// This is `MmzFlushCache(block, RK_FALSE)`.
    pub fn flush(&self) {
        unsafe { RK_MPI_SYS_MmzFlushCache(self.block, 0) };
    }
}

impl Drop for MmzBuffer {
    fn drop(&mut self) {
        unsafe { RK_MPI_SYS_MmzFree(self.block) };
    }
}

impl core::fmt::Debug for MmzBuffer {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("MmzBuffer")
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

/// A vendor block wrapping a buffer the engine owns.
///
/// The speaker path builds one of these around each block of samples the daemon
/// sent: the device reads from the block, so the borrow keeps the samples alive
/// until the block is released.
pub struct ExternalBlock<'a> {
    block: Block,
    buffer: &'a [u8],
}

impl<'a> ExternalBlock<'a> {
    /// Wrap `buffer` in a vendor block.
    pub fn new(buffer: &'a [u8]) -> Result<Self, Error> {
        let mut block: Block = ptr::null_mut();
        // The vendor interface takes a writable pointer even for playback, where
        // the device only reads; the shared borrow is what the engine actually
        // needs and what the type promises.
        let address = buffer.as_ptr() as *mut u8;
        let config = ExternalBlockConfig {
            virtual_address: address,
            reserved0: 0,
            physical_address: 0,
            descriptor: 0,
            reserved1: 0,
            size: buffer.len() as u64,
            free_callback: None,
            opaque: address as *mut c_void,
        };
        check(unsafe { RK_MPI_SYS_CreateMB(&mut block, &config) })?;
        if block.is_null() {
            return Err(Error::from_errno(libc::ENOMEM));
        }
        Ok(Self { block, buffer })
    }

    /// The vendor block, for handing to a device call.
    pub fn block(&self) -> Block {
        self.block
    }

    /// The wrapped bytes.
    pub fn buffer(&self) -> &[u8] {
        self.buffer
    }
}

impl Drop for ExternalBlock<'_> {
    fn drop(&mut self) {
        unsafe { RK_MPI_MB_ReleaseMB(self.block) };
    }
}

impl core::fmt::Debug for ExternalBlock<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ExternalBlock")
            .field("length", &self.buffer.len())
            .finish_non_exhaustive()
    }
}

/// An owned audio output device with its channel and resampler.
///
/// The reference engine sets these up one call at a time in `init_speaker` and
/// takes them down in `close_speaker`; keeping them in a value means a failure
/// halfway through a device that is only partly configured cannot leak it.
pub struct AudioOutput {
    device: c_int,
    channel: c_int,
    enabled: bool,
    channel_enabled: bool,
    resampling: bool,
}

impl AudioOutput {
    /// Configure, enable and start an output device.
    ///
    /// `input_rate` is the rate the engine feeds, which the resampler converts
    /// to the card's own rate.
    pub fn open(
        device: c_int,
        channel: c_int,
        attributes: &AudioAttributes,
        params: &AudioOutputChannelParams,
        input_rate: i32,
        volume: i32,
        track_mode: i32,
    ) -> Result<Self, Error> {
        let mut output = Self {
            device,
            channel,
            enabled: false,
            channel_enabled: false,
            resampling: false,
        };
        check(unsafe { RK_MPI_AO_SetPubAttr(device, attributes) })?;
        check(unsafe { RK_MPI_AO_Enable(device) })?;
        output.enabled = true;
        check(unsafe { RK_MPI_AO_SetChnParams(device, channel, params) })?;
        check(unsafe { RK_MPI_AO_EnableChn(device, channel) })?;
        output.channel_enabled = true;
        check(unsafe { RK_MPI_AO_EnableReSmp(device, channel, input_rate) })?;
        output.resampling = true;
        output.set_volume(volume)?;
        output.set_track_mode(track_mode)?;
        Ok(output)
    }

    /// Set the output volume.
    pub fn set_volume(&mut self, volume: i32) -> Result<(), Error> {
        check(unsafe { RK_MPI_AO_SetVolume(self.device, volume) })
    }

    /// Route the output to a track.
    pub fn set_track_mode(&mut self, mode: i32) -> Result<(), Error> {
        check(unsafe { RK_MPI_AO_SetTrackMode(self.device, mode) })
    }

    /// Play one block of PCM held in a vendor block.
    ///
    /// `length` is the length the device expects for the channel count: the
    /// reference engine passes the byte count for mono output and the byte count
    /// divided by the channel count otherwise.
    pub fn play(
        &mut self,
        block: Block,
        length: u32,
        sound_mode: i32,
        timestamp: u64,
        sequence: u32,
        timeout_ms: i32,
    ) -> Result<(), Error> {
        let frame = AudioFrame {
            block,
            bit_width: AUDIO_BIT_WIDTH_16,
            sound_mode,
            timestamp,
            sequence,
            length,
            bypass_block: 0,
        };
        check(unsafe { RK_MPI_AO_SendFrame(self.device, self.channel, &frame, timeout_ms) })
    }

    /// Discard whatever is queued for playback.
    pub fn clear(&mut self) -> Result<(), Error> {
        check(unsafe { RK_MPI_AO_ClearChnBuf(self.device, self.channel) })
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        // The reference engine clears the queue and disables the resampler and the
        // channel. Disabling the device as well is the counterpart of `Enable`,
        // which the reference leaves on - it lets the process exit with the
        // device still owned, so an engine that restarts playback in-process
        // would find it held.
        if self.channel_enabled {
            unsafe { RK_MPI_AO_ClearChnBuf(self.device, self.channel) };
        }
        if self.resampling {
            unsafe { RK_MPI_AO_DisableReSmp(self.device, self.channel) };
        }
        if self.channel_enabled {
            unsafe { RK_MPI_AO_DisableChn(self.device, self.channel) };
        }
        if self.enabled {
            unsafe { RK_MPI_AO_Disable(self.device) };
        }
    }
}

impl core::fmt::Debug for AudioOutput {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("AudioOutput")
            .field("device", &self.device)
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

/// Samples the microphone path assembles per frame (reference value).
pub const AUDIO_FRAME_SAMPLES: u32 = 320;

/// Largest microphone record worth sending.
///
/// The reference engine caps a captured block at 4096 bytes before it becomes a
/// record; the daemon accepts more for other kinds, but a G.711 block of that
/// size is already seconds of audio and anything larger is a broken device.
pub const AUDIO_MAX_PAYLOAD: usize = 4096;

/// Frames the encoder holds (reference value).
const AUDIO_ENCODER_BUFFER_COUNT: u32 = 4;

/// Depth of the encoder's output queue (reference value).
const AUDIO_ENCODER_DEPTH: u32 = 4;

/// Frames of input the capture channel keeps for the user (reference value).
const AUDIO_INPUT_FRAME_DEPTH: i32 = 1;

/// An owned audio input device with its capture channel.
pub struct AudioInput {
    device: c_int,
    channel: c_int,
    enabled: bool,
    channel_enabled: bool,
}

impl AudioInput {
    /// Configure, enable and start a capture device.
    pub fn open(
        device: c_int,
        channel: c_int,
        attributes: &AudioAttributes,
        params: &AudioInputChannelParams,
        track_mode: i32,
    ) -> Result<Self, Error> {
        let mut params = *params;
        let mut input = Self {
            device,
            channel,
            enabled: false,
            channel_enabled: false,
        };
        check(unsafe { RK_MPI_AI_SetPubAttr(device, attributes) })?;
        check(unsafe { RK_MPI_AI_Enable(device) })?;
        input.enabled = true;
        check(unsafe { RK_MPI_AI_SetChnParam(device, channel, &mut params) })?;
        check(unsafe { RK_MPI_AI_SetTrackMode(device, track_mode) })?;
        check(unsafe { RK_MPI_AI_EnableChn(device, channel) })?;
        input.channel_enabled = true;
        Ok(input)
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        // The reference engine disables the channel and the device; the resampler
        // and voice-enhancement paths the vendor supports are not enabled here.
        if self.channel_enabled {
            unsafe { RK_MPI_AI_DisableChn(self.device, self.channel) };
        }
        if self.enabled {
            unsafe { RK_MPI_AI_Disable(self.device) };
        }
    }
}

impl core::fmt::Debug for AudioInput {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("AudioInput")
            .field("device", &self.device)
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

/// An owned audio encoder channel.
pub struct AudioEncoder {
    channel: c_int,
    binding: Option<AudioBinding>,
}

impl AudioEncoder {
    /// Create an encoder channel with the given codec attributes.
    ///
    /// The attributes are copied onto the stack before the call, because the
    /// vendor entry point is handed a pointer it may write through. The reference
    /// engine passes a local structure for the same reason; handing it a `const`
    /// in read-only memory is a fault rather than an error code.
    pub fn open(channel: c_int, attributes: &AudioEncoderAttributes) -> Result<Self, Error> {
        let mut attributes = *attributes;
        check(unsafe { RK_MPI_AENC_CreateChn(channel, &mut attributes) })?;
        Ok(Self {
            channel,
            binding: None,
        })
    }

    /// Bind a capture channel to this encoder and take ownership of the edge.
    pub fn bind(mut self, source: Channel, destination: Channel) -> Result<Self, Error> {
        self.binding = Some(AudioBinding::open(source, destination)?);
        Ok(self)
    }

    /// Take the next encoded block, waiting up to `timeout_ms`.
    ///
    /// `None` covers a timeout, a failed read and an empty block alike: the
    /// reference engine treats every one of those the same way, by going round
    /// again.
    pub fn next_block(&mut self, timeout_ms: i32) -> Option<AudioBlock<'_>> {
        let mut stream = AudioStream {
            block: ptr::null_mut(),
            length: 0,
            timestamp: 0,
            sequence: 0,
            bypass_block: 0,
        };
        if unsafe { RK_MPI_AENC_GetStream(self.channel, &mut stream, timeout_ms) } != RK_SUCCESS {
            return None;
        }
        if stream.block.is_null() || stream.length == 0 {
            unsafe { RK_MPI_AENC_ReleaseStream(self.channel, &mut stream) };
            return None;
        }
        Some(AudioBlock {
            encoder: self,
            stream,
        })
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        // Remove the system edge before destroying the endpoint it targets.
        drop(self.binding.take());
        unsafe { RK_MPI_AENC_DestroyChn(self.channel) };
    }
}

impl core::fmt::Debug for AudioEncoder {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("AudioEncoder")
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

/// One encoded block, returned to the encoder when it goes out of scope.
pub struct AudioBlock<'a> {
    encoder: &'a mut AudioEncoder,
    stream: AudioStream,
}

impl AudioBlock<'_> {
    /// The encoded bytes.
    ///
    /// The encoder wrote this block, so unlike a captured video frame there is no
    /// device-side cache to invalidate before reading it - the reference engine
    /// reads it directly too.
    pub fn data(&self) -> &[u8] {
        let pointer = unsafe { RK_MPI_MB_Handle2VirAddr(self.stream.block) } as *const u8;
        if pointer.is_null() {
            return &[];
        }
        // SAFETY: the block holds `length` bytes that stay valid until the stream
        // is released, which cannot happen while this borrow exists.
        unsafe { core::slice::from_raw_parts(pointer, self.stream.length as usize) }
    }

    /// The device timestamp, or 0 when the encoder did not provide one.
    pub fn timestamp(&self) -> u64 {
        self.stream.timestamp
    }
}

impl Drop for AudioBlock<'_> {
    fn drop(&mut self) {
        unsafe { RK_MPI_AENC_ReleaseStream(self.encoder.channel, &mut self.stream) };
    }
}

impl core::fmt::Debug for AudioBlock<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("AudioBlock")
            .field("length", &self.stream.length)
            .field("timestamp", &self.stream.timestamp)
            .finish()
    }
}

/// An active capture-to-encoder binding.
///
/// A bound AENC channel cannot be safely destroyed while the system still owns
/// the edge. The reference's short-lived audio helper explicitly unbinds first;
/// keeping that edge owned here makes the same ordering unavoidable for the
/// long-lived microphone thread.
struct AudioBinding {
    source: Channel,
    destination: Channel,
}

impl AudioBinding {
    /// Create and own a capture-to-encoder edge.
    fn open(source: Channel, destination: Channel) -> Result<Self, Error> {
        check(unsafe { RK_MPI_SYS_Bind(&source, &destination) })?;
        Ok(Self {
            source,
            destination,
        })
    }
}

impl Drop for AudioBinding {
    fn drop(&mut self) {
        let _ = check(unsafe { RK_MPI_SYS_UnBind(&self.source, &self.destination) });
    }
}

/// The capture-channel parameters the reference engine uses: one frame of depth,
/// no loopback.
pub const INPUT_CHANNEL_PARAMS: AudioInputChannelParams = AudioInputChannelParams {
    user_frame_depth: AUDIO_INPUT_FRAME_DEPTH,
    loopback_mode: AUDIO_LOOPBACK_NONE,
};

/// The G.711 A-law encoder attributes the reference engine uses.
///
/// One channel at 8 kHz, coded at 8 bits per sample with a 64 kbit/s nominal
/// rate, which is what the daemon's RTSP track carries.
pub const ALAW_ENCODER_ATTRIBUTES: AudioEncoderAttributes = AudioEncoderAttributes {
    codec: AUDIO_ID_PCM_ALAW,
    buffer_count: AUDIO_ENCODER_BUFFER_COUNT,
    codec_attributes: AudioCodecAttributes {
        codec: AUDIO_ID_PCM_ALAW,
        bit_width: AUDIO_BIT_WIDTH_16,
        channels: 1,
        sample_rate: AUDIO_SAMPLE_RATE_8000 as u32,
        bits_per_coded_sample: ALAW_BITS_PER_SAMPLE,
        bitrate: ALAW_BITRATE_BPS,
        reserved: [0; 4],
        reserved_pointer: ptr::null_mut(),
    },
    depth: AUDIO_ENCODER_DEPTH,
};

/// Bits per coded A-law sample (reference value).
const ALAW_BITS_PER_SAMPLE: u32 = 8;

/// Nominal A-law bitrate in bits per second (reference value).
const ALAW_BITRATE_BPS: u32 = 64_000;

/// An owned video encoder channel.
pub struct EncoderChannel {
    channel: c_int,
    created: bool,
    receiving: bool,
}

impl EncoderChannel {
    /// Create a channel with the given attributes.
    ///
    /// Like the audio devices, the attributes are copied onto the stack first:
    /// the vendor entry points write through the structures they are handed, and
    /// read-only memory is a fault rather than an error code.
    pub fn create(channel: c_int, attributes: &ChannelAttributes) -> Result<Self, Error> {
        let mut attributes = *attributes;
        check(unsafe { RK_MPI_VENC_CreateChn(channel, &mut attributes) })?;
        let mut value = Self {
            channel,
            created: true,
            receiving: false,
        };
        // A channel that cannot start receiving frames is a failure: report it and
        // let `Drop` destroy the channel it just created.
        value.start_receiving()?;
        Ok(value)
    }

    /// Ask the channel to encode frames until it is stopped.
    pub fn start_receiving(&mut self) -> Result<(), Error> {
        let mut params = ReceivePictureParams {
            receive_picture_count: -1,
        };
        check(unsafe { RK_MPI_VENC_StartRecvFrame(self.channel, &mut params) })?;
        self.receiving = true;
        Ok(())
    }

    /// Encode one captured frame.
    ///
    /// The description is copied for the same reason the attributes are.
    pub fn send_frame(&mut self, frame: &VideoFrameInfo, timeout_ms: i32) -> Result<(), Error> {
        let mut frame = *frame;
        check(unsafe { RK_MPI_VENC_SendFrame(self.channel, &mut frame, timeout_ms) })
    }

    /// Collect the encoded output for one frame.
    pub fn get_stream<const PACKS: usize>(
        &mut self,
        buffer: &mut EncodedStreamBuffer<PACKS>,
        timeout_ms: i32,
    ) -> Result<(), Error> {
        buffer.reset();
        check(unsafe { RK_MPI_VENC_GetStream(self.channel, buffer.stream(), timeout_ms) })
    }

    /// Return the encoded output to the encoder.
    ///
    /// Taking `&mut` of the buffer is what makes the packs' contents unusable
    /// afterwards: the encoder may overwrite them as soon as this returns.
    pub fn release_stream<const PACKS: usize>(
        &mut self,
        buffer: &mut EncodedStreamBuffer<PACKS>,
    ) -> Result<(), Error> {
        check(unsafe { RK_MPI_VENC_ReleaseStream(self.channel, buffer.stream()) })
    }
}

impl Drop for EncoderChannel {
    fn drop(&mut self) {
        if self.receiving {
            unsafe { RK_MPI_VENC_StopRecvFrame(self.channel) };
        }
        if self.created {
            unsafe { RK_MPI_VENC_DestroyChn(self.channel) };
        }
    }
}

impl core::fmt::Debug for EncoderChannel {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("EncoderChannel")
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

/// Storage for one frame's worth of encoded packs.
///
/// The encoder takes the array's address and fills in as many packs as it needs,
/// so the array and the stream description have to live together; this type
/// keeps them paired and fixed in place.
pub struct EncodedStreamBuffer<const PACKS: usize> {
    packs: [EncodedPack; PACKS],
    stream: EncodedStream,
}

impl<const PACKS: usize> EncodedStreamBuffer<PACKS> {
    /// Create an empty buffer.
    pub fn new() -> Self {
        Self {
            packs: [EncodedPack {
                block: ptr::null_mut(),
                length: 0,
                pts: 0,
                frame_end: 0,
                stream_end: 0,
                reserved: [0; 112],
            }; PACKS],
            stream: EncodedStream {
                packs: ptr::null_mut(),
                pad: [0; 3],
                pack_count: 0,
                sequence: 0,
                reserved: [0; 376],
            },
        }
    }

    /// Point the stream description at this buffer's pack array.
    fn stream(&mut self) -> *mut EncodedStream {
        self.stream.packs = self.packs.as_mut_ptr();
        self.stream.pack_count = PACKS as u32;
        &mut self.stream
    }

    /// Forget the previous frame's packs.
    fn reset(&mut self) {
        for pack in &mut self.packs {
            pack.block = ptr::null_mut();
            pack.length = 0;
        }
    }

    /// The packs the encoder filled in, in order.
    pub fn packs(&self) -> &[EncodedPack] {
        let count = (self.stream.pack_count as usize).min(PACKS);
        &self.packs[..count]
    }
}

impl<const PACKS: usize> Default for EncodedStreamBuffer<PACKS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const PACKS: usize> core::fmt::Debug for EncodedStreamBuffer<PACKS> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("EncodedStreamBuffer")
            .field("capacity", &PACKS)
            .field("packs", &self.stream.pack_count)
            .finish()
    }
}

impl EncodedPack {
    /// The encoded bytes in this pack.
    ///
    /// `None` means the encoder produced an empty pack, which the reference
    /// engine also skips.
    pub fn data(&self) -> Option<&[u8]> {
        if self.block.is_null() || self.length == 0 {
            return None;
        }
        // The encoder wrote this buffer by DMA, so drop stale cache lines before
        // reading it (the reference engine's `MmzFlushCache(block, RK_TRUE)`).
        unsafe { RK_MPI_SYS_MmzFlushCache(self.block, 1) };
        let pointer = unsafe { RK_MPI_MB_Handle2VirAddr(self.block) } as *const u8;
        if pointer.is_null() {
            return None;
        }
        // SAFETY: the pack describes `length` bytes in a block the encoder owns
        // until the stream is released, and the caller cannot release it while it
        // holds this borrow.
        Some(unsafe { core::slice::from_raw_parts(pointer, self.length as usize) })
    }
}

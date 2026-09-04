//! FDK AAC decoding, for the talkback audio the daemon sends.
//!
//! The daemon may send talkback as raw PCM or as AAC access units; this is the
//! decoder for the second case. The library is FDK's, shipped on the camera as
//! `libfdk-aac.so.1`, and the reference engine opens it as an ADTS decoder.

use crate::platform::Error;

/// `TT_MP4_ADTS`: ADTS framing, which is what the talkback stream carries.
const TRANSPORT_ADTS: i32 = 2;

/// `AAC_DEC_OK`: the buffer holds a decoded frame.
const DECODER_OK: i32 = 0x0000;

/// `AAC_DEC_NOT_ENOUGH_BITS`: the access unit has been consumed and the next one
/// is needed.
const DECODER_NOT_ENOUGH_BITS: i32 = 0x1002;

/// One decoder instance, owned by the library.
#[repr(C)]
pub struct DecoderInstance {
    _private: [u8; 0],
}

/// The leading fields of FDK's `CStreamInfo` that the engine reads.
///
/// The library hands back a much larger structure; the three integers below are
/// the ones the reference engine reads, and they are the first fields of the
/// documented layout (`sampleRate`, `frameSize`, `numChannels`), so only that
/// prefix is declared here. The rest is FDK's internal state.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub sample_rate: i32,
    pub frame_size: i32,
    pub channels: i32,
}

unsafe extern "C" {
    fn aacDecoder_Open(transport: i32, layers: u32) -> *mut DecoderInstance;
    fn aacDecoder_Close(decoder: *mut DecoderInstance);
    fn aacDecoder_Fill(
        decoder: *mut DecoderInstance,
        buffer: *mut *mut u8,
        size: *const u32,
        valid: *mut u32,
    ) -> i32;
    fn aacDecoder_DecodeFrame(
        decoder: *mut DecoderInstance,
        time_data: *mut i16,
        time_data_size: i32,
        flags: u32,
    ) -> i32;
    fn aacDecoder_GetStreamInfo(decoder: *mut DecoderInstance) -> *mut StreamInfo;
}

/// What one decode attempt produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoded {
    /// A frame was written into the caller's buffer.
    Frame,
    /// The access unit is exhausted; more input is needed.
    NeedMoreInput,
}

/// An open AAC decoder.
pub struct Decoder {
    handle: *mut DecoderInstance,
}

impl Decoder {
    /// Open an ADTS decoder with a single layer.
    pub fn open() -> Result<Self, Error> {
        let handle = unsafe { aacDecoder_Open(TRANSPORT_ADTS, 1) };
        if handle.is_null() {
            return Err(Error::from_vendor_status(libc::ENOMEM));
        }
        Ok(Self { handle })
    }

    /// Hand one access unit to the decoder.
    ///
    /// Returns the number of bytes the decoder did not consume, which mirrors
    /// the library's own `bytesValid` output and is what the reference engine
    /// feeds back on the next call.
    pub fn fill(&mut self, access_unit: &[u8]) -> Result<usize, Error> {
        // The library takes arrays of buffers; for one access unit the
        // single-element form is what the reference engine passes too.
        let mut buffer = access_unit.as_ptr() as *mut u8;
        let size = access_unit.len() as u32;
        let mut valid = size;
        super::check(unsafe { aacDecoder_Fill(self.handle, &mut buffer, &size, &mut valid) })?;
        Ok(valid as usize)
    }

    /// Decode one frame into `pcm`.
    pub fn decode(&mut self, pcm: &mut [i16]) -> Result<Decoded, Error> {
        let Ok(size) = i32::try_from(pcm.len()) else {
            return Err(Error::from_errno(libc::EINVAL));
        };
        let status = unsafe { aacDecoder_DecodeFrame(self.handle, pcm.as_mut_ptr(), size, 0) };
        match status {
            DECODER_OK => Ok(Decoded::Frame),
            DECODER_NOT_ENOUGH_BITS => Ok(Decoded::NeedMoreInput),
            other => Err(Error::from_vendor_status(other)),
        }
    }

    /// The format of the last decoded frame.
    pub fn stream_info(&self) -> Option<StreamInfo> {
        let pointer = unsafe { aacDecoder_GetStreamInfo(self.handle) };
        if pointer.is_null() {
            return None;
        }
        // SAFETY: the library owns the structure and keeps it valid until the
        // decoder is closed; only the documented leading fields are read.
        Some(unsafe { *pointer })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { aacDecoder_Close(self.handle) };
    }
}

impl core::fmt::Debug for Decoder {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("Decoder")
    }
}

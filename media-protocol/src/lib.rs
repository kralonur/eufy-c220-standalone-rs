//! Camera-local media record framing.
//!
//! This is the binary boundary between the Rust firmware service and the
//! camera-side media engine that owns the Rockchip hardware. The header layout
//! and record-kind values were reverse-engineered from the legacy C engine and
//! are not a Eufy network or P2P protocol.
//!
//! The crate is `no_std` because the media engine links the camera's uClibc
//! runtime directly and must not carry a thread-local segment (the board's
//! dynamic loader faults on `PT_TLS` in the main executable). Both sides of the
//! socket therefore share this framing instead of duplicating the layout.

#![no_std]

extern crate alloc;

pub mod annex_b;

use alloc::vec::Vec;
use core::fmt;

/// Magic at the start of every camera-local media record.
pub const FRAME_MAGIC: [u8; 4] = *b"C220";

/// Size of the fixed camera-local record header, in bytes.
pub const FRAME_HEADER_LEN: usize = 20;

/// Maximum access-unit payload accepted in either direction.
///
/// This bound matches the validated 2K video access-unit guard used by the
/// reference media engine and prevents a corrupt length from causing an
/// unbounded allocation.
pub const MAX_PAYLOAD_LEN: usize = 4 * 1024 * 1024;

/// Record kinds in the camera-local media ABI.
///
/// Unknown values are retained so the framing layer can stay
/// forward-compatible while a higher layer decides whether to handle them.
/// ABI value 5 was a detection sub-stream that was never finished and is
/// deliberately no longer recognized, so such a record decodes as
/// [`RecordKind::Unknown`] and is ignored instead of joining the main stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Video,
    AudioCapture,
    SpeakerPcm,
    SpeakerClear,
    SpeakerVolume,
    NightMode,
    Unknown(u8),
}

impl RecordKind {
    pub const VIDEO: Self = Self::Video;
    pub const AUDIO_CAPTURE: Self = Self::AudioCapture;
    pub const SPEAKER_PCM: Self = Self::SpeakerPcm;
    pub const SPEAKER_CLEAR: Self = Self::SpeakerClear;
    pub const SPEAKER_VOLUME: Self = Self::SpeakerVolume;
    pub const NIGHT_MODE: Self = Self::NightMode;

    /// Return the ABI value written into the record header.
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Video => 0,
            Self::AudioCapture => 1,
            Self::SpeakerPcm => 2,
            Self::SpeakerClear => 3,
            Self::SpeakerVolume => 4,
            Self::NightMode => 6,
            Self::Unknown(value) => value,
        }
    }

    /// Decode an ABI value, preserving unrecognized kinds.
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Video,
            1 => Self::AudioCapture,
            2 => Self::SpeakerPcm,
            3 => Self::SpeakerClear,
            4 => Self::SpeakerVolume,
            6 => Self::NightMode,
            value => Self::Unknown(value),
        }
    }

    /// Whether this record carries an encoded video access unit.
    pub const fn is_video(self) -> bool {
        matches!(self, Self::Video)
    }
}

/// One validated camera-local record header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    payload_len: u32,
    timestamp_us: u64,
    keyframe: bool,
    kind: RecordKind,
    codec: u8,
}

impl Header {
    /// Build a header for an outgoing record, rejecting oversized payloads.
    pub fn new(
        payload_len: usize,
        timestamp_us: u64,
        keyframe: bool,
        kind: RecordKind,
        codec: u8,
    ) -> Result<Self, ProtocolError> {
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge(payload_len));
        }
        // Invariant: `MAX_PAYLOAD_LEN` fits a `u32`, so the cast is lossless.
        Ok(Self {
            payload_len: payload_len as u32,
            timestamp_us,
            keyframe,
            kind,
            codec,
        })
    }

    /// Parse and validate a header read from a stream.
    pub fn parse(bytes: &[u8; FRAME_HEADER_LEN]) -> Result<Self, ProtocolError> {
        if bytes[..4] != FRAME_MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let payload_len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if payload_len as usize > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge(payload_len as usize));
        }
        Ok(Self {
            payload_len,
            timestamp_us: u64::from_le_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ]),
            keyframe: bytes[16] != 0,
            kind: RecordKind::from_u8(bytes[17]),
            codec: bytes[18],
        })
    }

    /// Encode the header for transmission.
    pub fn encode(self) -> [u8; FRAME_HEADER_LEN] {
        let mut bytes = [0_u8; FRAME_HEADER_LEN];
        bytes[..4].copy_from_slice(&FRAME_MAGIC);
        bytes[4..8].copy_from_slice(&self.payload_len.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.timestamp_us.to_le_bytes());
        bytes[16] = u8::from(self.keyframe);
        bytes[17] = self.kind.as_u8();
        bytes[18] = self.codec;
        bytes
    }

    /// Payload length declared by this header.
    pub const fn payload_len(self) -> usize {
        self.payload_len as usize
    }

    /// Capture or command timestamp, in microseconds.
    pub const fn timestamp_us(self) -> u64 {
        self.timestamp_us
    }

    /// Whether the payload starts a new group of pictures.
    pub const fn keyframe(self) -> bool {
        self.keyframe
    }

    /// ABI record kind.
    pub const fn kind(self) -> RecordKind {
        self.kind
    }

    /// Raw codec byte.
    ///
    /// The native ABI reuses this byte for command payloads, so the framing
    /// layer deliberately preserves it instead of imposing a video-only enum.
    pub const fn codec(self) -> u8 {
        self.codec
    }
}

/// One camera-local media record.
///
/// Incoming records own their payload by default. Outgoing records can borrow
/// a slice or use a fixed-size array without allocating or copying it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaRecord<P = Vec<u8>> {
    pub kind: RecordKind,
    pub codec: u8,
    pub keyframe: bool,
    pub timestamp_us: u64,
    pub payload: P,
}

impl<P: AsRef<[u8]>> MediaRecord<P> {
    /// Build the validated header describing this record.
    pub fn header(&self) -> Result<Header, ProtocolError> {
        Header::new(
            self.payload.as_ref().len(),
            self.timestamp_us,
            self.keyframe,
            self.kind,
            self.codec,
        )
    }
}

/// A malformed or unsupported frame on the camera-local media socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// The record did not begin with [`FRAME_MAGIC`].
    BadMagic,
    /// The declared payload length exceeds [`MAX_PAYLOAD_LEN`].
    PayloadTooLarge(usize),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(formatter, "media record magic mismatch"),
            Self::PayloadTooLarge(length) => write!(
                formatter,
                "media record payload length {length} exceeds {MAX_PAYLOAD_LEN}"
            ),
        }
    }
}

impl core::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(record: &MediaRecord) -> MediaRecord {
        let mut encoded = Vec::new();
        let header = record.header().expect("record should encode a header");
        encoded.extend_from_slice(&header.encode());
        encoded.extend_from_slice(&record.payload);

        let mut header_bytes = [0_u8; FRAME_HEADER_LEN];
        header_bytes.copy_from_slice(&encoded[..FRAME_HEADER_LEN]);
        let parsed = Header::parse(&header_bytes).expect("header should decode");
        MediaRecord {
            kind: parsed.kind(),
            codec: parsed.codec(),
            keyframe: parsed.keyframe(),
            timestamp_us: parsed.timestamp_us(),
            payload: encoded[FRAME_HEADER_LEN..].to_vec(),
        }
    }

    #[test]
    fn round_trips_video_record() {
        let original = MediaRecord {
            kind: RecordKind::VIDEO,
            codec: 2,
            keyframe: true,
            timestamp_us: 66_666,
            payload: alloc::vec![0, 0, 0, 1, 0x40, 0x01],
        };
        assert_eq!(round_trip(&original), original);
    }

    #[test]
    fn preserves_unknown_record_kinds() {
        let record = MediaRecord {
            kind: RecordKind::Unknown(99),
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: alloc::vec![1, 2, 3],
        };
        assert_eq!(round_trip(&record).kind, RecordKind::Unknown(99));
    }

    #[test]
    fn accepts_empty_control_payload() {
        let record = MediaRecord {
            kind: RecordKind::SPEAKER_CLEAR,
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: Vec::new(),
        };
        assert_eq!(round_trip(&record), record);
        assert_eq!(record.header().expect("header").payload_len(), 0);
    }

    #[test]
    fn rejects_bad_magic_and_oversized_payload_before_allocating() {
        let mut bad_magic = [0_u8; FRAME_HEADER_LEN];
        bad_magic[4..8].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(Header::parse(&bad_magic), Err(ProtocolError::BadMagic));

        let mut oversized = [0_u8; FRAME_HEADER_LEN];
        oversized[..4].copy_from_slice(&FRAME_MAGIC);
        oversized[4..8].copy_from_slice(&((MAX_PAYLOAD_LEN as u32) + 1).to_le_bytes());
        assert_eq!(
            Header::parse(&oversized),
            Err(ProtocolError::PayloadTooLarge(MAX_PAYLOAD_LEN + 1))
        );

        let record = MediaRecord {
            kind: RecordKind::VIDEO,
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: alloc::vec![0; MAX_PAYLOAD_LEN + 1],
        };
        assert_eq!(
            record.header(),
            Err(ProtocolError::PayloadTooLarge(MAX_PAYLOAD_LEN + 1))
        );
    }

    #[test]
    fn maps_record_kinds_to_their_abi_values() {
        for (kind, value) in [
            (RecordKind::VIDEO, 0),
            (RecordKind::AUDIO_CAPTURE, 1),
            (RecordKind::SPEAKER_PCM, 2),
            (RecordKind::SPEAKER_CLEAR, 3),
            (RecordKind::SPEAKER_VOLUME, 4),
            (RecordKind::NIGHT_MODE, 6),
            (RecordKind::Unknown(200), 200),
        ] {
            assert_eq!(kind.as_u8(), value);
            assert_eq!(RecordKind::from_u8(value), kind);
        }
        assert!(RecordKind::VIDEO.is_video());
        assert!(!RecordKind::AUDIO_CAPTURE.is_video());
    }

    #[test]
    fn treats_the_retired_sub_stream_kind_as_unknown() {
        // ABI value 5 belonged to the unfinished sub-stream. Decoding it as an
        // unknown kind keeps an older engine from injecting a second video
        // resolution into the single RTSP video track.
        assert_eq!(RecordKind::from_u8(5), RecordKind::Unknown(5));
        assert!(!RecordKind::Unknown(5).is_video());
    }
}

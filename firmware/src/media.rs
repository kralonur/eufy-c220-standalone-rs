//! Camera-local media record framing.
//!
//! This is the binary boundary between the Rust firmware service and the
//! camera's native hardware media producer. The header layout and record-kind
//! values were reverse-engineered from the reference
//! `native/c220-media-engine.c` and validated in `docs/hardware-video.md`.
//! It is not an Eufy network or P2P protocol.

use std::io::{self, Read, Write};

/// Magic at the start of every camera-local media record.
pub const FRAME_MAGIC: [u8; 4] = *b"C220";

/// Size of the fixed camera-local record header, in bytes.
pub const FRAME_HEADER_LEN: usize = 20;

/// Maximum access-unit payload accepted from the hardware producer.
///
/// This bound matches the validated 2K video access-unit guard used by the
/// reference media engine and prevents a corrupt length from causing an
/// unbounded allocation.
pub const MAX_PAYLOAD_LEN: usize = 4 * 1024 * 1024;

/// Record kinds in the camera-local media ABI.
///
/// The numeric values are RE-derived from the native companion. Unknown values
/// are retained so the framing layer can stay forward-compatible while a
/// higher layer decides whether to handle them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Video,
    AudioCapture,
    SpeakerPcm,
    SpeakerClear,
    SpeakerVolume,
    VideoSub,
    NightMode,
    Unknown(u8),
}

impl RecordKind {
    pub const VIDEO: Self = Self::Video;
    pub const AUDIO_CAPTURE: Self = Self::AudioCapture;
    pub const SPEAKER_PCM: Self = Self::SpeakerPcm;
    pub const SPEAKER_CLEAR: Self = Self::SpeakerClear;
    pub const SPEAKER_VOLUME: Self = Self::SpeakerVolume;
    pub const VIDEO_SUB: Self = Self::VideoSub;
    pub const NIGHT_MODE: Self = Self::NightMode;

    /// Return the ABI value written into the record header.
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Video => 0,
            Self::AudioCapture => 1,
            Self::SpeakerPcm => 2,
            Self::SpeakerClear => 3,
            Self::SpeakerVolume => 4,
            Self::VideoSub => 5,
            Self::NightMode => 6,
            Self::Unknown(value) => value,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Video,
            1 => Self::AudioCapture,
            2 => Self::SpeakerPcm,
            3 => Self::SpeakerClear,
            4 => Self::SpeakerVolume,
            5 => Self::VideoSub,
            6 => Self::NightMode,
            value => Self::Unknown(value),
        }
    }

    /// Whether this record carries an encoded video access unit.
    pub const fn is_video(self) -> bool {
        matches!(self, Self::Video | Self::VideoSub)
    }
}

/// One camera-local media record.
#[derive(Debug, PartialEq, Eq)]
pub struct MediaRecord {
    /// Record kind from header byte 17.
    pub kind: RecordKind,
    /// Raw codec byte from header byte 18.
    ///
    /// The native ABI reuses this byte for command payloads, so the framing
    /// layer deliberately preserves it instead of imposing a video-only enum.
    pub codec: u8,
    /// Keyframe flag from header byte 16.
    pub keyframe: bool,
    /// Capture or command timestamp from header bytes 8..16, in microseconds.
    pub timestamp_us: u64,
    /// Record payload following the fixed header.
    pub payload: Vec<u8>,
}

impl MediaRecord {
    /// Read one record from a byte stream.
    ///
    /// `Ok(None)` means the stream reached EOF before another header began.
    /// A truncated header or payload is an error because it makes the stream
    /// boundary unusable.
    pub fn read_from<R: Read>(reader: &mut R) -> io::Result<Option<Self>> {
        let mut header = [0_u8; FRAME_HEADER_LEN];
        match reader.read(&mut header[..1])? {
            0 => return Ok(None),
            1 => {}
            _ => {}
        }
        reader.read_exact(&mut header[1..])?;

        if header[..4] != FRAME_MAGIC {
            return Err(invalid_data("media record magic mismatch"));
        }

        let payload_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(invalid_data(format!(
                "media record payload length {payload_len} exceeds {MAX_PAYLOAD_LEN}"
            )));
        }

        let mut payload = vec![0_u8; payload_len];
        reader.read_exact(&mut payload)?;

        Ok(Some(Self {
            kind: RecordKind::from_u8(header[17]),
            codec: header[18],
            keyframe: header[16] != 0,
            timestamp_us: u64::from_le_bytes([
                header[8], header[9], header[10], header[11], header[12], header[13], header[14],
                header[15],
            ]),
            payload,
        }))
    }

    /// Write one complete record to a byte stream.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        if self.payload.len() > MAX_PAYLOAD_LEN {
            return Err(invalid_input(format!(
                "media record payload length {} exceeds {MAX_PAYLOAD_LEN}",
                self.payload.len()
            )));
        }

        let payload_len = u32::try_from(self.payload.len()).map_err(|_| {
            invalid_input(format!(
                "media record payload length {} does not fit the ABI",
                self.payload.len()
            ))
        })?;

        let mut header = [0_u8; FRAME_HEADER_LEN];
        header[..4].copy_from_slice(&FRAME_MAGIC);
        header[4..8].copy_from_slice(&payload_len.to_le_bytes());
        header[8..16].copy_from_slice(&self.timestamp_us.to_le_bytes());
        header[16] = u8::from(self.keyframe);
        header[17] = self.kind.as_u8();
        header[18] = self.codec;

        writer.write_all(&header)?;
        writer.write_all(&self.payload)
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trips_video_record() {
        let original = MediaRecord {
            kind: RecordKind::VIDEO,
            codec: 2,
            keyframe: true,
            timestamp_us: 66_666,
            payload: vec![0, 0, 0, 1, 0x40, 0x01],
        };
        let mut encoded = Vec::new();

        original
            .write_to(&mut encoded)
            .expect("record should encode");
        let decoded = MediaRecord::read_from(&mut Cursor::new(encoded))
            .expect("record should decode")
            .expect("record should be present");

        assert_eq!(decoded, original);
    }

    #[test]
    fn preserves_unknown_record_kinds() {
        let record = MediaRecord {
            kind: RecordKind::Unknown(99),
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: vec![1, 2, 3],
        };
        let mut encoded = Vec::new();

        record.write_to(&mut encoded).expect("record should encode");
        let decoded = MediaRecord::read_from(&mut Cursor::new(encoded))
            .expect("record should decode")
            .expect("record should be present");

        assert_eq!(decoded.kind, RecordKind::Unknown(99));
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
        let mut encoded = Vec::new();

        record.write_to(&mut encoded).expect("record should encode");
        assert_eq!(encoded.len(), FRAME_HEADER_LEN);
        assert_eq!(
            MediaRecord::read_from(&mut Cursor::new(encoded))
                .expect("record should decode")
                .expect("record should be present"),
            record
        );
    }

    #[test]
    fn rejects_bad_magic_and_oversized_payload_before_allocating() {
        let mut bad_magic = vec![0_u8; FRAME_HEADER_LEN];
        bad_magic[4..8].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(
            MediaRecord::read_from(&mut Cursor::new(bad_magic))
                .expect_err("bad magic must fail")
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mut oversized = Vec::from(FRAME_MAGIC);
        oversized.extend_from_slice(&((MAX_PAYLOAD_LEN as u32) + 1).to_le_bytes());
        oversized.extend_from_slice(&[0_u8; 12]);
        assert_eq!(
            MediaRecord::read_from(&mut Cursor::new(oversized))
                .expect_err("oversized payload must fail")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn clean_eof_returns_none() {
        assert_eq!(
            MediaRecord::read_from(&mut Cursor::new(Vec::<u8>::new()))
                .expect("EOF is not an error"),
            None
        );
    }
}

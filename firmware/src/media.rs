//! Camera-local media record framing over std byte streams.
//!
//! The wire layout lives in the `c220-media-protocol` crate so that the media
//! engine, which is `no_std`, frames records exactly like this service does.
//! This module only adds the blocking `std::io` adapter.

pub use c220_media_protocol::{MediaRecord, RecordKind};

use c220_media_protocol::{FRAME_HEADER_LEN, Header, ProtocolError};
use std::io::{self, Read, Write};

/// Read one record from a byte stream.
///
/// `Ok(None)` means the stream reached EOF before another header began.
/// A truncated header or payload is an error because it makes the stream
/// boundary unusable.
pub fn read_record<R: Read>(reader: &mut R) -> io::Result<Option<MediaRecord>> {
    let mut header = [0_u8; FRAME_HEADER_LEN];
    if reader.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..])?;

    let header = Header::parse(&header).map_err(invalid_data)?;
    let mut payload = vec![0_u8; header.payload_len()];
    reader.read_exact(&mut payload)?;

    Ok(Some(MediaRecord {
        kind: header.kind(),
        codec: header.codec(),
        keyframe: header.keyframe(),
        timestamp_us: header.timestamp_us(),
        payload,
    }))
}

/// Write one complete record to a byte stream.
pub fn write_record<W: Write>(record: &MediaRecord, writer: &mut W) -> io::Result<()> {
    let header = record.header().map_err(invalid_input)?;
    writer.write_all(&header.encode())?;
    writer.write_all(&record.payload)
}

fn invalid_data(error: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn invalid_input(error: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use c220_media_protocol::{FRAME_MAGIC, MAX_PAYLOAD_LEN};
    use std::io::Cursor;

    #[test]
    fn round_trips_a_record_through_a_stream() {
        let original = MediaRecord {
            kind: RecordKind::VIDEO,
            codec: 2,
            keyframe: true,
            timestamp_us: 66_666,
            payload: vec![0, 0, 0, 1, 0x40, 0x01],
        };
        let mut encoded = Vec::new();

        write_record(&original, &mut encoded).expect("record should encode");
        let decoded = read_record(&mut Cursor::new(encoded))
            .expect("record should decode")
            .expect("record should be present");

        assert_eq!(decoded, original);
    }

    #[test]
    fn reports_clean_eof_and_stream_errors() {
        assert_eq!(
            read_record(&mut Cursor::new(Vec::<u8>::new())).expect("EOF is not an error"),
            None
        );

        let mut bad_magic = vec![0_u8; FRAME_HEADER_LEN];
        bad_magic[4..8].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(
            read_record(&mut Cursor::new(bad_magic))
                .expect_err("bad magic must fail")
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mut oversized = Vec::from(FRAME_MAGIC);
        oversized.extend_from_slice(&((MAX_PAYLOAD_LEN as u32) + 1).to_le_bytes());
        oversized.extend_from_slice(&[0_u8; 12]);
        assert_eq!(
            read_record(&mut Cursor::new(oversized))
                .expect_err("oversized payload must fail")
                .kind(),
            io::ErrorKind::InvalidData
        );

        let oversized_record = MediaRecord {
            kind: RecordKind::VIDEO,
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: vec![0_u8; MAX_PAYLOAD_LEN + 1],
        };
        assert_eq!(
            write_record(&oversized_record, &mut Vec::new())
                .expect_err("oversized payload must be rejected")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

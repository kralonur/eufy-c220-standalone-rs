//! Camera-local media record framing over std byte streams.
//!
//! The wire layout lives in the `c220-media-protocol` crate so that the media
//! engine, which is `no_std`, frames records exactly like this service does.
//! This module only adds the blocking `std::io` adapter.

pub use c220_media_protocol::{MediaRecord, RecordKind};

use c220_media_protocol::{FRAME_HEADER_LEN, Header, ProtocolError};
use std::io::{self, IoSlice, Read, Write};

/// Read one record from a byte stream.
///
/// `Ok(None)` means the stream reached EOF before another header began.
/// A truncated header or payload is an error because it makes the stream
/// boundary unusable.
pub fn read_record<R: Read>(reader: &mut R) -> io::Result<Option<MediaRecord>> {
    let mut header = [0_u8; FRAME_HEADER_LEN];
    let received = loop {
        match reader.read(&mut header) {
            Ok(0) => return Ok(None),
            Ok(count) => break count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    };
    reader.read_exact(&mut header[received..])?;

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
pub fn write_record<W: Write, P: AsRef<[u8]>>(
    record: &MediaRecord<P>,
    writer: &mut W,
) -> io::Result<()> {
    let header = record.header().map_err(invalid_input)?.encode();
    let mut slices = [IoSlice::new(&header), IoSlice::new(record.payload.as_ref())];
    let mut remaining = &mut slices[..];
    // A vectored write can stop in either slice. Keep the record boundary
    // intact even for writers that only implement scalar writes.
    while !remaining.is_empty() {
        match writer.write_vectored(remaining) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => IoSlice::advance_slices(&mut remaining, written),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
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

    struct ShortIo {
        bytes: Cursor<Vec<u8>>,
        limit: usize,
        interrupted: bool,
    }

    impl Read for ShortIo {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if std::mem::take(&mut self.interrupted) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = output.len().min(self.limit);
            self.bytes.read(&mut output[..count])
        }
    }

    impl Write for ShortIo {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            self.write_vectored(&[IoSlice::new(input)])
        }

        fn write_vectored(&mut self, input: &[IoSlice<'_>]) -> io::Result<usize> {
            if std::mem::take(&mut self.interrupted) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let mut remaining = self.limit;
            let mut written = 0;
            for slice in input {
                let count = remaining.min(slice.len());
                self.bytes.write_all(&slice[..count])?;
                remaining -= count;
                written += count;
            }
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // Short writes can stop inside either iovec or cross their boundary;
    // interruptions at the first header byte must not be reported as EOF.
    #[test]
    fn round_trips_borrowed_records_with_short_and_interrupted_io() {
        for payload in [&[][..], &[0xab; 31][..]] {
            let record = MediaRecord {
                kind: RecordKind::SPEAKER_PCM,
                codec: 0,
                keyframe: true,
                timestamp_us: 20_000,
                payload,
            };
            for limit in [
                1,
                FRAME_HEADER_LEN - 1,
                FRAME_HEADER_LEN,
                FRAME_HEADER_LEN + 1,
            ] {
                let mut stream = ShortIo {
                    bytes: Cursor::new(Vec::new()),
                    limit,
                    interrupted: true,
                };
                write_record(&record, &mut stream).expect("short vectored writes should finish");
                stream.bytes.set_position(0);
                stream.interrupted = true;
                let decoded = read_record(&mut stream)
                    .expect("interrupted reads should retry")
                    .expect("record should be present");
                assert_eq!(decoded.header(), record.header());
                assert_eq!(decoded.payload, payload);
                assert_eq!(read_record(&mut stream).expect("EOF"), None);
            }
        }
        let record = MediaRecord {
            kind: RecordKind::SPEAKER_CLEAR,
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: &[],
        };
        let mut stream = ShortIo {
            bytes: Cursor::new(Vec::new()),
            limit: 0,
            interrupted: false,
        };
        assert_eq!(
            write_record(&record, &mut stream)
                .expect_err("zero write must fail")
                .kind(),
            io::ErrorKind::WriteZero
        );
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

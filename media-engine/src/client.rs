//! Record I/O for the daemon connection.
//!
//! The engine and the firmware share the framing itself (the
//! `c220-media-protocol` crate); this module is the blocking socket side of it,
//! with the reference engine's read semantics: a partial header or payload ends
//! the session, and a read that is interrupted or times out is retried until the
//! engine is asked to stop.

use crate::platform::Error;
use crate::platform::net::UnixStream;
use c220_media_protocol::{FRAME_HEADER_LEN, Header, RecordKind};

/// Sends engine records to the connected daemon.
#[derive(Clone, Copy)]
pub struct RecordWriter<'a> {
    stream: &'a UnixStream,
}

impl<'a> RecordWriter<'a> {
    /// Wrap the connection a session is being served on.
    pub fn new(stream: &'a UnixStream) -> Self {
        Self { stream }
    }

    /// Write one record.
    pub fn send(
        &self,
        kind: RecordKind,
        codec: u8,
        keyframe: bool,
        timestamp_us: u64,
        payload: &[u8],
    ) -> Result<(), Error> {
        write_record(self.stream, kind, codec, keyframe, timestamp_us, payload)
    }

    /// Write the night-mode state the daemon expects on connect.
    ///
    /// The payload is the current mode and whether it was chosen manually, which
    /// is what the reference engine sends at the start of every client session.
    pub fn send_night_mode(&self, night: bool, manual: bool) -> Result<(), Error> {
        let payload = [u8::from(night), u8::from(manual)];
        self.send(RecordKind::NIGHT_MODE, 0, false, 0, &payload)
    }
}

/// How a blocking read finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The buffer was filled.
    Filled,
    /// The session ended: the daemon closed the socket or a stop was requested.
    Ended,
}

/// The first part of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderRead {
    /// A validated header.
    Header(Header),
    /// The daemon closed the socket, or a stop was requested.
    Ended,
    /// The bytes were not a record header.
    Malformed,
}

/// Write one record to the daemon.
///
/// The header is built here so an oversized payload is rejected before anything
/// reaches the socket.
fn write_record(
    stream: &UnixStream,
    kind: RecordKind,
    codec: u8,
    keyframe: bool,
    timestamp_us: u64,
    payload: &[u8],
) -> Result<(), Error> {
    let header = match Header::new(payload.len(), timestamp_us, keyframe, kind, codec) {
        Ok(header) => header,
        Err(error) => {
            crate::platform::media_log!("media_record_rejected={}", error);
            return Err(Error::from_errno(libc::EINVAL));
        }
    };
    stream.write_all(&header.encode())?;
    stream.write_all(payload)
}

/// Read one record header.
pub fn read_header<F: Fn() -> bool>(stream: &UnixStream, stop: F) -> Result<HeaderRead, Error> {
    let mut bytes = [0_u8; FRAME_HEADER_LEN];
    match read_exact(stream, &mut bytes, stop)? {
        Outcome::Ended => return Ok(HeaderRead::Ended),
        Outcome::Filled => {}
    }
    match Header::parse(&bytes) {
        Ok(header) => Ok(HeaderRead::Header(header)),
        Err(_) => Ok(HeaderRead::Malformed),
    }
}

/// Fill `buffer` completely, retrying interrupted and timed-out reads.
pub fn read_exact<F: Fn() -> bool>(
    stream: &UnixStream,
    buffer: &mut [u8],
    stop: F,
) -> Result<Outcome, Error> {
    let mut filled = 0;
    while filled < buffer.len() {
        // The stop flag is checked between reads, so a stop requested while the
        // daemon is silent still ends the session promptly.
        if stop() {
            return Ok(Outcome::Ended);
        }
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => return Ok(Outcome::Ended),
            Ok(count) => filled += count,
            Err(error) if error.is_interrupted() || error.would_block() => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(Outcome::Filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::net::UnixListener;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream as StdStream;

    fn connected_pair(name: &str) -> (UnixStream, StdStream, alloc::string::String) {
        let path = alloc::format!(
            "/tmp/c220-media-engine-{}-{}.sock",
            std::process::id(),
            name
        );
        let listener = UnixListener::bind(&path).expect("listener should bind");
        let client = std::thread::spawn({
            let path = path.clone();
            move || StdStream::connect(&path).expect("client should connect")
        });
        let server = listener.accept().expect("accept should succeed");
        let client = client.join().expect("client thread");
        (server, client, path)
    }

    #[test]
    fn writes_records_the_daemon_can_parse() {
        let (server, mut client, path) = connected_pair("write");
        let writer = RecordWriter::new(&server);

        writer
            .send_night_mode(true, false)
            .expect("night mode should write");
        writer
            .send(RecordKind::SPEAKER_CLEAR, 0, false, 0, &[])
            .expect("clear should write");

        let mut bytes = [0_u8; FRAME_HEADER_LEN];
        client.read_exact(&mut bytes).expect("header should read");
        let header = Header::parse(&bytes).expect("header should parse");
        assert_eq!(header.kind(), RecordKind::NIGHT_MODE);
        assert_eq!(header.payload_len(), 2);
        let mut payload = [0_u8; 2];
        client
            .read_exact(&mut payload)
            .expect("payload should read");
        assert_eq!(payload, [1, 0]);

        client.read_exact(&mut bytes).expect("header should read");
        let header = Header::parse(&bytes).expect("header should parse");
        assert_eq!(header.kind(), RecordKind::SPEAKER_CLEAR);
        assert_eq!(header.payload_len(), 0);

        std::fs::remove_file(&path).expect("socket file should be removable");
    }

    #[test]
    fn reads_a_record_the_daemon_sent() {
        let (server, mut client, path) = connected_pair("read");

        let payload = [7_u8, 0, 0, 0];
        let header =
            Header::new(payload.len(), 0, false, RecordKind::SPEAKER_VOLUME, 0).expect("header");
        client
            .write_all(&header.encode())
            .expect("header should write");
        client.write_all(&payload).expect("payload should write");

        let header = match read_header(&server, || false).expect("read should succeed") {
            HeaderRead::Header(header) => header,
            other => panic!("expected a header, got {other:?}"),
        };
        assert_eq!(header.kind(), RecordKind::SPEAKER_VOLUME);
        let mut received = [0_u8; 4];
        assert_eq!(
            read_exact(&server, &mut received, || false).expect("read should succeed"),
            Outcome::Filled
        );
        assert_eq!(received, payload);

        std::fs::remove_file(&path).expect("socket file should be removable");
    }

    #[test]
    fn reports_malformed_and_incomplete_records() {
        let (server, mut client, path) = connected_pair("malformed");

        client.write_all(&[0_u8; FRAME_HEADER_LEN]).expect("bytes");
        assert_eq!(
            read_header(&server, || false).expect("read should succeed"),
            HeaderRead::Malformed
        );

        // Half a header is an ended session, not a partial record.
        client.write_all(b"C").expect("byte should write");
        drop(client);
        assert_eq!(
            read_header(&server, || false).expect("read should succeed"),
            HeaderRead::Ended
        );

        std::fs::remove_file(&path).expect("socket file should be removable");
    }

    #[test]
    fn a_requested_stop_ends_the_session() {
        let (server, client, path) = connected_pair("stop");

        // Nothing was written by the daemon: the read must not block forever once
        // the stop flag is set.
        assert_eq!(
            read_header(&server, || true).expect("read should succeed"),
            HeaderRead::Ended
        );

        drop(client);
        std::fs::remove_file(&path).expect("socket file should be removable");
    }
}

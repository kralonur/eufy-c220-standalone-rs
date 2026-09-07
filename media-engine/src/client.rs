//! Record I/O for the daemon connection.
//!
//! The engine and the firmware share the framing itself (the
//! `c220-media-protocol` crate); this module is the socket side of it. Reads
//! belong to the session thread, which serves commands, and writes can come from
//! anywhere - the session thread's own greeting and night-mode records, and the
//! producer threads for microphone and video - so writes go through [`RecordSink`],
//! which holds the connection and one lock.

use crate::platform::Error;
use crate::platform::net::{self, UnixStream};
use crate::platform::sync::Mutex;
use c220_media_protocol::{FRAME_HEADER_LEN, Header, ProtocolError, RecordKind};
use core::sync::atomic::{AtomicI32, Ordering};

/// The descriptor value that means "no daemon connected".
const NO_DESCRIPTOR: i32 = -1;

/// The connected daemon, as record producers see it.
///
/// The reference engine keeps the same thing in a global descriptor plus
/// `client_lock`; keeping them together means the descriptor cannot change while
/// a record is half-written, and that a detached sink is never a stale one.
pub struct RecordSink {
    descriptor: AtomicI32,
    writes: Mutex<()>,
}

impl RecordSink {
    /// Create a sink with no daemon attached.
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            descriptor: AtomicI32::new(NO_DESCRIPTOR),
            writes: Mutex::new(())?,
        })
    }

    /// Whether a daemon is connected.
    pub fn connected(&self) -> bool {
        self.descriptor.load(Ordering::SeqCst) != NO_DESCRIPTOR
    }

    /// Point the sink at the session's socket.
    pub fn attach(&self, stream: &UnixStream) -> Result<(), Error> {
        // Taking the lock means a write already in flight finishes before the
        // descriptor changes.
        let _writes = self.writes.lock()?;
        self.descriptor.store(stream.raw_fd(), Ordering::SeqCst);
        Ok(())
    }

    /// Forget the session, before its socket is closed.
    pub fn detach(&self) -> Result<(), Error> {
        let _writes = self.writes.lock()?;
        self.descriptor.store(NO_DESCRIPTOR, Ordering::SeqCst);
        Ok(())
    }

    /// Write one record.
    ///
    /// A missing daemon is not an error: a producer simply has nowhere to send
    /// its record, which is what the reference engine's `client_current() >= 0`
    /// check amounts to.
    pub fn send(
        &self,
        kind: RecordKind,
        codec: u8,
        keyframe: bool,
        timestamp_us: u64,
        payload: &[u8],
    ) -> Result<(), Error> {
        let _writes = self.writes.lock()?;
        let descriptor = self.descriptor.load(Ordering::SeqCst);
        if descriptor == NO_DESCRIPTOR {
            return Ok(());
        }
        write_record_to(descriptor, kind, codec, keyframe, timestamp_us, payload)
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

/// Write one record to a connected descriptor.
///
/// The header is built here so an oversized payload is rejected before anything
/// reaches the socket.
fn write_record_to(
    descriptor: i32,
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
    net::write_all(descriptor, &header.encode())?;
    net::write_all(descriptor, payload)
}

/// The first part of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderRead {
    /// A validated header.
    Header(Header),
    /// The daemon closed the socket, or a stop was requested.
    Ended,
    /// The bytes were not a record header, for the reason given.
    Malformed(ProtocolError),
}

/// How a blocking read finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The buffer was filled.
    Filled,
    /// The session ended: the daemon closed the socket or a stop was requested.
    Ended,
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
        Err(error) => Ok(HeaderRead::Malformed(error)),
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
    use std::sync::Arc;

    fn socket_path(name: &str) -> alloc::string::String {
        alloc::format!("/tmp/c220-media-engine-{}-{name}.sock", std::process::id())
    }

    fn connected_pair(name: &str) -> (UnixStream, StdStream, alloc::string::String) {
        let path = socket_path(name);
        let listener = UnixListener::bind(&path).expect("listener should bind");
        let client = std::thread::spawn({
            let path = path.clone();
            move || StdStream::connect(&path).expect("client should connect")
        });
        let server = listener.accept().expect("accept should succeed");
        let client = client.join().expect("client thread");
        (server, client, path)
    }

    /// Read one record from the daemon's side of the connection.
    fn read_record(client: &mut StdStream) -> (Header, Vec<u8>) {
        let mut bytes = [0_u8; FRAME_HEADER_LEN];
        client.read_exact(&mut bytes).expect("header should read");
        let header = Header::parse(&bytes).expect("header should parse");
        let mut payload = alloc::vec![0_u8; header.payload_len()];
        client
            .read_exact(&mut payload)
            .expect("payload should read");
        (header, payload)
    }

    #[test]
    fn sends_records_the_daemon_can_parse() {
        let (server, mut client, path) = connected_pair("sink-write");
        let sink = RecordSink::new().expect("sink should initialize");
        sink.attach(&server).expect("attach should succeed");
        assert!(sink.connected());

        sink.send_night_mode(true, false)
            .expect("night mode should write");
        sink.send(RecordKind::SPEAKER_CLEAR, 0, false, 0, &[])
            .expect("clear should write");

        let (header, payload) = read_record(&mut client);
        assert_eq!(header.kind(), RecordKind::NIGHT_MODE);
        assert_eq!(payload, [1, 0]);
        let (header, payload) = read_record(&mut client);
        assert_eq!(header.kind(), RecordKind::SPEAKER_CLEAR);
        assert!(payload.is_empty());

        std::fs::remove_file(&path).expect("socket file should be removable");
    }

    #[test]
    fn a_detached_sink_sends_nothing() {
        let (server, mut client, path) = connected_pair("sink-detached");
        let sink = RecordSink::new().expect("sink should initialize");

        // Nothing is attached: this is what a producer does before a daemon
        // connects, and it must not be an error.
        assert!(!sink.connected());
        sink.send_night_mode(true, true)
            .expect("an unattached send is not an error");

        sink.attach(&server).expect("attach should succeed");
        sink.send_night_mode(false, true)
            .expect("attached send should work");
        sink.detach().expect("detach should succeed");
        sink.send_night_mode(true, false)
            .expect("a send after detach is not an error");

        // Only the attached record arrived.
        let (header, payload) = read_record(&mut client);
        assert_eq!(header.kind(), RecordKind::NIGHT_MODE);
        assert_eq!(payload, [0, 1]);

        drop(client);
        std::fs::remove_file(&path).expect("socket file should be removable");
    }

    #[test]
    fn producers_can_share_one_sink() {
        let (server, mut client, path) = connected_pair("sink-shared");
        let sink = Arc::new(RecordSink::new().expect("sink should initialize"));
        sink.attach(&server).expect("attach should succeed");

        // Small enough that neither producer can fill the socket buffer and
        // block before the test reads.
        const RECORDS: usize = 20;
        let producers: Vec<_> = (0..2)
            .map(|_| {
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    for _ in 0..RECORDS {
                        sink.send(RecordKind::AUDIO_CAPTURE, 0, false, 0, &[0xab; 320])
                            .expect("send should succeed");
                    }
                })
            })
            .collect();
        for producer in producers {
            producer.join().expect("producer should finish");
        }

        // Every record arrives whole: the header and its payload are never
        // interleaved with another thread's.
        for _ in 0..RECORDS * 2 {
            let (header, payload) = read_record(&mut client);
            assert_eq!(header.kind(), RecordKind::AUDIO_CAPTURE);
            assert_eq!(payload.len(), 320);
            assert!(payload.iter().all(|byte| *byte == 0xab));
        }

        drop(client);
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

        client
            .write_all(&[0_u8; FRAME_HEADER_LEN])
            .expect("bytes should write");
        assert_eq!(
            read_header(&server, || false).expect("read should succeed"),
            HeaderRead::Malformed(ProtocolError::BadMagic)
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

//! End-to-end test of a daemon session on a real socket.
//!
//! The engine is bound and served in a thread while the test plays the firmware:
//! connect, read the greeting, send commands and watch a malformed session end.
//! This is the code the camera runs, with only the backend mocked.

use c220_media_engine::backend::{Backend, MockBackend};
use c220_media_engine::cli::EngineConfig;
use c220_media_engine::client::RecordSink;
use c220_media_engine::commands::Command;
use c220_media_engine::engine::Engine;
use c220_media_engine::platform::Error;
use c220_media_engine::platform::signal::Shutdown;
use c220_media_protocol::{FRAME_HEADER_LEN, Header, RecordKind};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// A running engine and the socket it serves.
struct Harness {
    path: String,
    stop: Arc<Shutdown>,
    session: JoinHandle<()>,
}

impl Harness {
    fn start(name: &str) -> Self {
        Self::start_with_backend(name, MockBackend::new())
    }

    fn start_with_backend(name: &str, mut backend: impl Backend + Send + 'static) -> Self {
        let path = format!(
            "/tmp/c220-media-engine-session-{}-{name}.sock",
            std::process::id()
        );
        let config = EngineConfig {
            socket_path: path.clone(),
            ..EngineConfig::default()
        };
        let mut engine = Engine::bind(&config).expect("engine should bind");
        let stop = engine.stop_handle();
        let session = std::thread::spawn(move || {
            engine.serve(&mut backend);
            backend.shutdown();
        });
        Self {
            path,
            stop,
            session,
        }
    }

    fn connect(&self) -> UnixStream {
        let client = UnixStream::connect(&self.path).expect("client should connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout should be accepted");
        client
    }

    fn finish(self) {
        self.stop.request();
        self.session.join().expect("session thread");
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read one whole record from the engine.
fn read_record(client: &mut UnixStream) -> (RecordKind, Vec<u8>) {
    let mut bytes = [0_u8; FRAME_HEADER_LEN];
    client.read_exact(&mut bytes).expect("header should read");
    let header = Header::parse(&bytes).expect("header should parse");
    let mut payload = vec![0_u8; header.payload_len()];
    client
        .read_exact(&mut payload)
        .expect("payload should read");
    (header.kind(), payload)
}

/// Send one whole record to the engine.
fn send_record(
    client: &mut UnixStream,
    kind: RecordKind,
    keyframe: bool,
    timestamp_us: u64,
    payload: &[u8],
) {
    let header =
        Header::new(payload.len(), timestamp_us, keyframe, kind, 0).expect("header should build");
    client
        .write_all(&header.encode())
        .expect("header should write");
    client.write_all(payload).expect("payload should write");
}

#[test]
fn greets_a_new_session_and_reports_night_mode_changes() {
    let harness = Harness::start("greeting");
    let mut client = harness.connect();

    // A fresh engine starts in day mode, choosing for itself.
    assert_eq!(
        read_record(&mut client),
        (RecordKind::NIGHT_MODE, vec![0, 0])
    );

    // Speaker commands are accepted without a reply; the next record the test
    // sees must be the night-mode change, not an echo.
    send_record(
        &mut client,
        RecordKind::SPEAKER_VOLUME,
        false,
        0,
        &42_u32.to_le_bytes(),
    );
    send_record(&mut client, RecordKind::SPEAKER_CLEAR, false, 0, &[]);
    send_record(
        &mut client,
        RecordKind::SPEAKER_PCM,
        true,
        1_000,
        &[0_u8; 640],
    );
    send_record(
        &mut client,
        RecordKind::SPEAKER_PCM,
        false,
        2_000,
        &[0_u8; 256],
    );

    // Forcing night mode is reported as (night, chosen manually).
    send_record(&mut client, RecordKind::NIGHT_MODE, false, 0, &[1]);
    assert_eq!(
        read_record(&mut client),
        (RecordKind::NIGHT_MODE, vec![1, 1])
    );

    // Forcing day mode is reported the same way.
    send_record(&mut client, RecordKind::NIGHT_MODE, false, 0, &[2]);
    assert_eq!(
        read_record(&mut client),
        (RecordKind::NIGHT_MODE, vec![0, 1])
    );

    drop(client);
    harness.finish();
}

#[test]
fn serves_a_second_session_after_the_first_ends() {
    let harness = Harness::start("sequential");

    let mut first = harness.connect();
    assert_eq!(
        read_record(&mut first),
        (RecordKind::NIGHT_MODE, vec![0, 0])
    );
    // Leave the engine in night mode, then hang up.
    send_record(&mut first, RecordKind::NIGHT_MODE, false, 0, &[1]);
    assert_eq!(
        read_record(&mut first),
        (RecordKind::NIGHT_MODE, vec![1, 1])
    );
    drop(first);

    // The next session starts with the state the engine kept.
    let mut second = harness.connect();
    assert_eq!(
        read_record(&mut second),
        (RecordKind::NIGHT_MODE, vec![1, 1])
    );
    drop(second);

    harness.finish();
}

struct FloodingBackend {
    inner: MockBackend,
    go: Option<std::sync::mpsc::Receiver<()>>,
    producer: Option<JoinHandle<()>>,
}

impl Backend for FloodingBackend {
    fn start(&mut self, sink: Arc<RecordSink>, _: Arc<Shutdown>) -> Result<(), Error> {
        let go = self.go.take().expect("one start");
        self.producer = Some(std::thread::spawn(move || {
            go.recv().expect("start producer after greeting");
            let payload = vec![0xab; c220_media_protocol::MAX_PAYLOAD_LEN];
            let _ = sink.send(RecordKind::VIDEO, 2, true, 0, &payload);
        }));
        Ok(())
    }

    fn command(&mut self, command: Command<'_>, sink: &RecordSink) -> Result<(), Error> {
        self.inner.command(command, sink)
    }

    fn night_mode(&self) -> (bool, bool) {
        self.inner.night_mode()
    }

    fn shutdown(&mut self) {
        if let Some(producer) = self.producer.take() {
            producer.join().expect("producer should exit");
        }
    }
}

// Rejection must wake a video producer blocked in a full send buffer before
// detaching its locked sink. Keeping the first peer open reproduces the deadlock.
#[test]
fn a_malformed_session_releases_a_backpressured_producer() {
    let (go, receiver) = std::sync::mpsc::channel();
    let backend = FloodingBackend {
        inner: MockBackend::new(),
        go: Some(receiver),
        producer: None,
    };
    let harness = Harness::start_with_backend("backpressure", backend);
    let mut first = harness.connect();
    assert_eq!(read_record(&mut first).0, RecordKind::NIGHT_MODE);
    go.send(()).expect("start video producer");
    let mut header = [0; FRAME_HEADER_LEN];
    first.read_exact(&mut header).expect("video write started");
    assert_eq!(
        Header::parse(&header).expect("video header").payload_len(),
        c220_media_protocol::MAX_PAYLOAD_LEN
    );
    // Do not drain the multi-megabyte payload. The producer is blocked while
    // the independent session reader rejects this deliberately corrupt header.
    first
        .write_all(&[0; FRAME_HEADER_LEN])
        .expect("bad command header");
    let mut second = harness.connect();
    assert_eq!(read_record(&mut second).0, RecordKind::NIGHT_MODE);
    drop(first);
    drop(second);
    harness.finish();
}

#[test]
fn a_malformed_session_ends_without_taking_the_engine_down() {
    // Each case has to end the session, and the engine has to keep serving
    // afterwards - that is what lets the firmware reconnect after a bad record.
    // The first three are refused from the header alone; the last one is a
    // well-sized record whose body breaks the command ABI, so its body is sent.
    let cases: [(&str, Vec<u8>); 4] = [
        ("bad-magic", vec![0_u8; FRAME_HEADER_LEN]),
        (
            "volume-too-long",
            header_only(RecordKind::SPEAKER_VOLUME, false, 5),
        ),
        (
            "pcm-too-large",
            header_only(RecordKind::SPEAKER_PCM, true, 64 * 1024 + 1),
        ),
        (
            "volume-too-short",
            body_record(RecordKind::SPEAKER_VOLUME, false, &[0, 0, 0]),
        ),
    ];

    for (name, bytes) in cases {
        let harness = Harness::start(name);
        let mut client = harness.connect();
        assert_eq!(
            read_record(&mut client),
            (RecordKind::NIGHT_MODE, vec![0, 0])
        );
        client.write_all(&bytes).expect("record should write");

        let mut buffer = [0_u8; 1];
        match client.read(&mut buffer) {
            Ok(0) => {}
            other => panic!("{name}: expected the engine to end the session, got {other:?}"),
        }
        drop(client);

        // The engine is still there for the next daemon.
        let mut next = harness.connect();
        assert_eq!(
            read_record(&mut next),
            (RecordKind::NIGHT_MODE, vec![0, 0]),
            "{name}: engine should accept a new session"
        );
        drop(next);

        harness.finish();
    }
}

/// Build a header announcing `payload_len` bytes and no body: the engine has to
/// refuse these from the header alone, without reading a payload it must not
/// allocate for.
fn header_only(kind: RecordKind, keyframe: bool, payload_len: usize) -> Vec<u8> {
    let header = Header::new(payload_len, 0, keyframe, kind, 0).expect("header should build");
    header.encode().to_vec()
}

/// Build a complete record whose body is what makes it unservable.
fn body_record(kind: RecordKind, keyframe: bool, payload: &[u8]) -> Vec<u8> {
    let header = Header::new(payload.len(), 0, keyframe, kind, 0).expect("header should build");
    let mut bytes = header.encode().to_vec();
    bytes.extend_from_slice(payload);
    bytes
}

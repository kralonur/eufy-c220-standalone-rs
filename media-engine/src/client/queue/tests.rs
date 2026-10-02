use super::*;
use std::io::Read;
use std::time::{Duration, Instant};

// A blocked socket must not block vendor capture, exceed the byte budget, or
// survive its IPC deadline while retaining a frame allocation.
#[test]
fn stalled_daemon_bounds_capture_memory_and_expires_the_writer() {
    let (server, mut client, path) = crate::client::tests::connected_pair("queued-backpressure");
    client
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let sink = RecordSink::new().unwrap();
    sink.attach(&server).unwrap();
    let payload = vec![0xab; c220_media_protocol::MAX_PAYLOAD_LEN];
    sink.send(RecordKind::VIDEO, 2, true, 0, &payload).unwrap();
    // Receiving the header proves the writer is in this record; stop draining
    // its multi-megabyte body so it cannot finish the send.
    let mut header = [0; FRAME_HEADER_LEN];
    client.read_exact(&mut header).unwrap();
    let began = Instant::now();
    for _ in 0..20 {
        sink.send(RecordKind::VIDEO, 2, true, 0, &payload[..512 * 1024])
            .unwrap();
    }
    assert!(began.elapsed() < Duration::from_secs(1));
    {
        let queue = sink.shared.queue.lock().unwrap();
        assert!(queue.bytes <= QUEUE_BYTES);
        assert!(queue.video.len() <= VIDEO_RECORDS);
        assert!(queue.other.len() <= OTHER_RECORDS);
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    while sink.connected() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!sink.connected(), "stalled IPC writer must disconnect");
    assert_eq!(sink.shared.queue.lock().unwrap().bytes, 0);
    sink.detach().unwrap();
    drop(client);
    std::fs::remove_file(path).unwrap();
}

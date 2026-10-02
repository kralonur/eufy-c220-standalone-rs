//! Bounded copies decouple vendor capture threads from a stalled daemon. Only
//! the writer touches the socket; queue locks are never held during network I/O.
use crate::platform::{
    Error,
    event::Event,
    net::{self, UnixStream},
    sync::Mutex,
    thread::{self, Thread},
};
use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use c220_media_protocol::{FRAME_HEADER_LEN, Header, RecordKind};
use core::sync::atomic::{AtomicBool, Ordering};

/// Six pending video records bound the C220's concurrent capture producers.
const VIDEO_RECORDS: usize = 6;
/// Three captured audio records plus room for current night-state feedback.
const OTHER_RECORDS: usize = 4;
/// Six MiB includes pending and in-flight encoded copies and their wire headers.
const QUEUE_BYTES: usize = 6 * 1024 * 1024;

struct Pending {
    header: Header,
    payload: Box<[u8]>,
    generation: u64,
}
impl Pending {
    fn bytes(&self) -> usize {
        FRAME_HEADER_LEN + self.payload.len()
    }
}

struct Queue {
    video: VecDeque<Pending>,
    other: VecDeque<Pending>,
    bytes: usize,
    generation: u64,
    waiting_for_keyframe: bool,
}
impl Queue {
    fn discard_video(&mut self) {
        for record in self.video.drain(..) {
            self.bytes -= record.bytes();
        }
        self.waiting_for_keyframe = true;
    }
    fn clear(&mut self) {
        self.discard_video();
        for record in self.other.drain(..) {
            self.bytes -= record.bytes();
        }
        self.generation = self.generation.wrapping_add(1);
    }
}
struct Shared {
    queue: Mutex<Queue>,
    connection: Mutex<Option<UnixStream>>,
    connected: AtomicBool,
    closed: AtomicBool,
    wake: Event,
}

/// Producers enqueue complete records; dropping AV data on overload is nonfatal.
/// Night-state feedback is coalesced to its latest value, not silently evicted by
/// audio. Each attachment owns a duplicated fd, preventing recycled-fd writes.
pub struct RecordSink {
    shared: Arc<Shared>,
    writer: Thread,
}
impl RecordSink {
    pub fn new() -> Result<Self, Error> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                video: VecDeque::with_capacity(VIDEO_RECORDS),
                other: VecDeque::with_capacity(OTHER_RECORDS),
                bytes: 0,
                generation: 0,
                waiting_for_keyframe: true,
            })?,
            connection: Mutex::new(None)?,
            connected: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            wake: Event::new()?,
        });
        let writer_shared = Arc::clone(&shared);
        let writer = thread::spawn("media-writer", move || {
            if let Err(error) = write_loop(&writer_shared) {
                crate::platform::media_log!("media_writer_error={error}");
            }
        })?;
        Ok(Self { shared, writer })
    }
    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }
    pub fn attach(&self, stream: &UnixStream) -> Result<(), Error> {
        let mut connection = self.shared.connection.lock()?;
        self.shared.queue.lock()?.clear();
        *connection = Some(stream.try_clone()?);
        self.shared.connected.store(true, Ordering::Release);
        Ok(())
    }
    pub fn detach(&self) -> Result<(), Error> {
        let mut connection = self.shared.connection.lock()?;
        self.shared.connected.store(false, Ordering::Release);
        self.shared.queue.lock()?.clear();
        *connection = None;
        Ok(())
    }
    pub fn send(
        &self,
        kind: RecordKind,
        codec: u8,
        keyframe: bool,
        timestamp_us: u64,
        payload: &[u8],
    ) -> Result<(), Error> {
        if !self.connected() {
            return Ok(());
        }
        let header = Header::new(payload.len(), timestamp_us, keyframe, kind, codec)
            .map_err(|_| Error::from_errno(libc::EINVAL))?;
        let cost = FRAME_HEADER_LEN + payload.len();
        let mut queue = self.shared.queue.lock()?;
        if !self.connected() {
            return Ok(());
        }
        if kind == RecordKind::VIDEO {
            if queue.video.len() == VIDEO_RECORDS || queue.bytes + cost > QUEUE_BYTES {
                queue.discard_video();
            }
            if (queue.waiting_for_keyframe && !keyframe) || queue.bytes + cost > QUEUE_BYTES {
                return Ok(());
            }
            queue.waiting_for_keyframe = false;
        } else {
            // Retain the latest state independently of an audio backlog.
            if kind == RecordKind::NIGHT_MODE
                && let Some(index) = queue
                    .other
                    .iter()
                    .position(|record| record.header.kind() == kind)
                && let Some(old) = queue.other.remove(index)
            {
                queue.bytes -= old.bytes();
            }
            if queue.other.len() == OTHER_RECORDS {
                if let Some(index) = queue
                    .other
                    .iter()
                    .position(|record| record.header.kind() == RecordKind::AUDIO_CAPTURE)
                {
                    if let Some(old) = queue.other.remove(index) {
                        queue.bytes -= old.bytes();
                    }
                } else {
                    return Err(Error::from_errno(libc::EAGAIN));
                }
            }
            if queue.bytes + cost > QUEUE_BYTES {
                queue.discard_video();
            }
            if queue.bytes + cost > QUEUE_BYTES {
                return Ok(());
            }
        }
        let record = Pending {
            header,
            payload: payload.into(),
            generation: queue.generation,
        };
        queue.bytes += cost;
        if kind == RecordKind::VIDEO {
            queue.video.push_back(record);
        } else {
            queue.other.push_back(record);
        }
        drop(queue);
        self.shared.wake.notify()
    }
    pub fn send_night_mode(&self, night: bool, manual: bool) -> Result<(), Error> {
        self.send(
            RecordKind::NIGHT_MODE,
            0,
            false,
            0,
            &[u8::from(night), u8::from(manual)],
        )
    }
}
impl Drop for RecordSink {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        let _ = self.shared.wake.notify();
        // The writer owns Shared, not RecordSink, so there is no Arc cycle or
        // self-join. A current write finishes or hits its bounded IPC deadline.
        self.writer.join();
    }
}
#[cfg(test)]
mod tests;

fn write_loop(shared: &Shared) -> Result<(), Error> {
    while !shared.closed.load(Ordering::Acquire) {
        let record = {
            let mut queue = shared.queue.lock()?;
            queue.other.pop_front().or_else(|| queue.video.pop_front())
        };
        let Some(record) = record else {
            shared.wake.wait()?;
            continue;
        };
        let mut connection = shared.connection.lock()?;
        let current = shared.queue.lock()?.generation == record.generation;
        let result = match connection.as_ref().filter(|_| current) {
            Some(stream) => {
                net::write_all_vectored(stream.raw_fd(), &record.header.encode(), &record.payload)
            }
            None => Ok(()),
        };
        let mut queue = shared.queue.lock()?;
        let cost = record.bytes();
        // Release accounting only after the allocation is actually freed.
        drop(record);
        queue.bytes -= cost;
        if let Err(error) = result {
            crate::platform::media_log!("media_client_write_error={error}");
            if let Some(stream) = connection.take() {
                stream.shutdown();
            }
            shared.connected.store(false, Ordering::Release);
            queue.clear();
        }
    }
    Ok(())
}

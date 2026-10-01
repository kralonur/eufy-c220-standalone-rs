//! Owner for the camera-local native media engine.
//!
//! The companion process owns the sensor/ISP/V4L2/VENC hardware and exposes
//! encoded records over a private Unix socket. This Rust boundary keeps the
//! reverse-engineered launch arguments and shutdown behavior of the legacy C
//! engine; the companion is part of our camera firmware, not a vendor/P2P
//! client.

use crate::media::{MediaRecord, RecordKind, read_record, write_record};
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Default path for the custom camera-side media companion.
pub const DEFAULT_PROGRAM: &str = "/home/c220-media-engine";

/// Default private socket shared by the daemon and the media companion.
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/c220-media.sock";

/// Validated 2K self-path capture node from the camera hardware evidence.
pub const DEFAULT_VIDEO_DEVICE: &str = "/dev/video12";

/// Factory audio card name used by the unified media companion.
pub const DEFAULT_AUDIO_CARD: &str = "hw:0,0";

/// Directory containing the camera's preserved AIQ calibration files.
pub const DEFAULT_IQ_DIRECTORY: &str = "/home";

/// Time allowed for the companion to create and listen on its socket.
const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Delay between socket connection attempts while the companion initializes.
const SOCKET_CONNECT_RETRY: Duration = Duration::from_millis(100);

/// Poll interval while waiting for a gracefully terminating child.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Grace period for the companion to deinitialize Rockchip media resources.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// POSIX signal used for graceful process termination.
const SIGTERM: i32 = 15;

/// POSIX signal used only when graceful termination exceeds [`SHUTDOWN_GRACE`].
const SIGKILL: i32 = 9;

/// POSIX error returned when a process no longer exists.
const ESRCH: i32 = 3;

/// Maximum raw PCM16 payload accepted by the native speaker path.
const MAX_SPEAKER_PCM_PAYLOAD: usize = 64 * 1024;

/// Maximum AAC payload accepted by the native decoder path.
const MAX_SPEAKER_AAC_PAYLOAD: usize = 48 * 1024;

/// Maximum volume accepted by the native AO implementation.
const MAX_SPEAKER_VOLUME: u32 = 100;

/// Sample rate expected by the native AO resampler for direct PCM playback.
const SPEAKER_PCM_SAMPLE_RATE: u64 = 16_000;

/// Number of microseconds in one second for speaker timestamps.
const MICROSECONDS_PER_SECOND: u64 = 1_000_000;

/// Configuration for the camera-local media companion.
#[derive(Clone, Debug)]
pub struct MediaEngineConfig {
    /// Executable that owns the native sensor/ISP/VENC integration.
    pub program: PathBuf,
    /// Private Unix socket created by the companion.
    pub socket_path: PathBuf,
    /// V4L2 capture node passed to the companion.
    pub video_device: PathBuf,
    /// Audio card passed to the companion.
    pub audio_card: String,
    /// Directory containing the AIQ calibration/configuration files.
    pub iq_directory: PathBuf,
    /// Whether the native companion should open the microphone capture path.
    pub audio_enabled: bool,
}

impl Default for MediaEngineConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::from(DEFAULT_PROGRAM),
            socket_path: PathBuf::from(DEFAULT_SOCKET_PATH),
            video_device: PathBuf::from(DEFAULT_VIDEO_DEVICE),
            audio_card: DEFAULT_AUDIO_CARD.to_string(),
            iq_directory: PathBuf::from(DEFAULT_IQ_DIRECTORY),
            audio_enabled: true,
        }
    }
}

/// A cross-task shutdown request for a media companion owned by a blocking
/// task.
#[derive(Clone, Default)]
pub struct MediaEngineStopHandle {
    child_pid: Arc<AtomicU32>,
    socket: Arc<Mutex<Option<UnixStream>>>,
}

impl MediaEngineStopHandle {
    fn set_child(&self, child: &Child) {
        self.child_pid.store(child.id(), Ordering::Release);
    }

    fn set_socket(&self, socket: &UnixStream) -> io::Result<()> {
        let socket = socket.try_clone()?;
        let mut current = self
            .socket
            .lock()
            .map_err(|_| io::Error::other("media stop socket lock poisoned"))?;
        *current = Some(socket);
        Ok(())
    }

    fn clear(&self) {
        self.child_pid.store(0, Ordering::Release);
        if let Ok(mut socket) = self.socket.lock() {
            *socket = None;
        }
    }

    /// Wake a blocked media read and ask the companion to terminate.
    pub fn request_shutdown(&self) {
        if let Ok(socket) = self.socket.lock()
            && let Some(socket) = socket.as_ref()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
        let child_pid = self.child_pid.load(Ordering::Acquire);
        if child_pid != 0 {
            let _ = send_signal(child_pid, SIGTERM);
        }
    }
}

/// Owns one media companion process and its bidirectional command/record
/// socket.
///
/// There is deliberately one owner of both the child and socket. The media
/// engine writes video/audio records to the daemon and the daemon writes
/// speaker/night-mode commands back over the same Unix stream.
pub struct MediaEngine {
    child: Child,
    socket: UnixStream,
    commands: MediaEngineHandle,
    stop_handle: MediaEngineStopHandle,
}

/// Thread-safe writer for commands sent to the media companion.
///
/// The companion's Unix socket is full duplex: its reader emits capture records
/// while its writer accepts speaker and camera-control records. All command
/// writes share one lock so a control request cannot interleave its header or
/// payload with a talkback frame.
#[derive(Clone)]
pub struct MediaEngineHandle {
    socket: Arc<Mutex<UnixStream>>,
    next_speaker_timestamp_us: Arc<AtomicU64>,
}

impl MediaEngine {
    /// Launch the companion and connect to its private socket.
    pub fn start(
        config: MediaEngineConfig,
        stop_handle: MediaEngineStopHandle,
    ) -> io::Result<Self> {
        let mut child = launch(&config)?;
        stop_handle.set_child(&child);
        let socket = match connect_until_ready(&config.socket_path, &mut child) {
            Ok(socket) => socket,
            Err(error) => {
                let error_kind = error.kind();
                let message = format!("media engine failed to become ready: {error}");
                let cleanup_result = terminate_child(&mut child);
                stop_handle.clear();
                if let Err(cleanup_error) = cleanup_result {
                    return Err(io::Error::new(
                        error_kind,
                        format!("{message}; failed to terminate child: {cleanup_error}"),
                    ));
                }
                return Err(io::Error::new(error_kind, message));
            }
        };

        let command_socket = match socket.try_clone() {
            Ok(command_socket) => command_socket,
            Err(error) => {
                let _ = socket.shutdown(Shutdown::Both);
                let _ = terminate_child(&mut child);
                stop_handle.clear();
                return Err(error);
            }
        };
        if let Err(error) = stop_handle.set_socket(&socket) {
            let _ = socket.shutdown(Shutdown::Both);
            let _ = terminate_child(&mut child);
            stop_handle.clear();
            return Err(error);
        }

        Ok(Self {
            child,
            socket,
            commands: MediaEngineHandle {
                socket: Arc::new(Mutex::new(command_socket)),
                next_speaker_timestamp_us: Arc::new(AtomicU64::new(0)),
            },
            stop_handle,
        })
    }

    /// Read the next record emitted by the companion.
    pub fn read_record(&mut self) -> io::Result<Option<MediaRecord>> {
        read_record(&mut self.socket)
    }

    /// Return the command writer used by control and talkback services.
    pub fn command_handle(&self) -> MediaEngineHandle {
        self.commands.clone()
    }

    /// Send one command record to the companion.
    pub fn write_record(&mut self, record: &MediaRecord) -> io::Result<()> {
        self.commands.write_record(record)
    }

    /// Request the camera speaker to discard queued playback.
    pub fn clear_speaker(&mut self) -> io::Result<()> {
        self.commands.clear_speaker()
    }

    /// Stop the companion and reap it before returning.
    pub fn shutdown(&mut self) -> io::Result<()> {
        let socket_result = self.socket.shutdown(Shutdown::Both);
        let child_result = terminate_child(&mut self.child);

        let result = match (socket_result, child_result) {
            (_, Err(error)) => Err(error),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        };
        self.stop_handle.clear();
        result
    }
}

impl Drop for MediaEngine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl MediaEngineHandle {
    #[cfg(test)]
    pub(crate) fn from_socket_for_test(socket: UnixStream) -> Self {
        Self {
            socket: Arc::new(Mutex::new(socket)),
            next_speaker_timestamp_us: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Send one command record to the companion.
    pub fn write_record<P: AsRef<[u8]>>(&self, record: &MediaRecord<P>) -> io::Result<()> {
        let mut socket = self
            .socket
            .lock()
            .map_err(|_| io::Error::other("media command socket lock poisoned"))?;
        write_record(record, &mut *socket)
    }

    /// Request the camera speaker to discard queued playback.
    pub fn clear_speaker(&self) -> io::Result<()> {
        self.write_record(&MediaRecord {
            kind: RecordKind::SPEAKER_CLEAR,
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: &[],
        })
    }

    /// Send signed little-endian PCM16 samples to the native speaker path.
    pub fn send_speaker_pcm(&self, pcm: &[u8], timestamp_us: u64) -> io::Result<()> {
        if pcm.is_empty() || pcm.len() > MAX_SPEAKER_PCM_PAYLOAD || !pcm.len().is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "speaker PCM must be non-empty, even-sized, and at most {MAX_SPEAKER_PCM_PAYLOAD} bytes"
                ),
            ));
        }
        self.write_record(&MediaRecord {
            kind: RecordKind::SPEAKER_PCM,
            codec: 0,
            keyframe: true,
            timestamp_us,
            payload: pcm,
        })
    }

    /// Send one native AAC access unit to the speaker decoder.
    pub fn send_speaker_aac(&self, aac: &[u8], timestamp_us: u64) -> io::Result<()> {
        if aac.is_empty() || aac.len() > MAX_SPEAKER_AAC_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "speaker AAC must be non-empty and at most {MAX_SPEAKER_AAC_PAYLOAD} bytes"
                ),
            ));
        }
        self.write_record(&MediaRecord {
            kind: RecordKind::SPEAKER_PCM,
            codec: 0,
            keyframe: false,
            timestamp_us,
            payload: aac,
        })
    }

    /// Set the native AO volume in its validated 0..=100 range.
    pub fn set_speaker_volume(&self, volume: u32) -> io::Result<()> {
        if volume > MAX_SPEAKER_VOLUME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("speaker volume {volume} exceeds {MAX_SPEAKER_VOLUME}"),
            ));
        }
        self.write_record(&MediaRecord {
            kind: RecordKind::SPEAKER_VOLUME,
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: volume.to_le_bytes(),
        })
    }

    /// Set the native night-mode target: 0=auto, 1=night, 2=day.
    pub fn set_night_mode(&self, mode: u8) -> io::Result<()> {
        if mode > 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("night mode {mode} is not one of auto/night/day"),
            ));
        }
        self.write_record(&MediaRecord {
            kind: RecordKind::NIGHT_MODE,
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: [mode],
        })
    }

    fn reserve_speaker_timestamp(&self, duration_us: u64) -> u64 {
        let mut current = self.next_speaker_timestamp_us.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(duration_us);
            match self.next_speaker_timestamp_us.compare_exchange(
                current,
                next,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => return current,
                Err(actual) => current = actual,
            }
        }
    }
}

impl crate::rtsp::TalkbackSink for MediaEngineHandle {
    fn start_talkback(&self) -> io::Result<()> {
        self.clear_speaker()
    }

    fn send_pcm(&self, pcm_bytes: &[u8]) -> io::Result<()> {
        let sample_count = u64::try_from(pcm_bytes.len() / 2).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "talkback PCM sample count overflows",
            )
        })?;
        let duration_us =
            sample_count.saturating_mul(MICROSECONDS_PER_SECOND) / SPEAKER_PCM_SAMPLE_RATE;
        let timestamp_us = self.reserve_speaker_timestamp(duration_us);
        self.send_speaker_pcm(pcm_bytes, timestamp_us)
    }

    fn stop_talkback(&self) -> io::Result<()> {
        self.clear_speaker()
    }
}

fn launch(config: &MediaEngineConfig) -> io::Result<Child> {
    let mut command = Command::new(&config.program);
    command
        .arg("--socket")
        .arg(&config.socket_path)
        .arg("--video-device")
        .arg(&config.video_device)
        .arg("--card")
        .arg(&config.audio_card)
        .arg("--iq-dir")
        .arg(&config.iq_directory);
    if !config.audio_enabled {
        command.arg("--no-audio");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
}

fn connect_until_ready(path: &Path, child: &mut Child) -> io::Result<UnixStream> {
    let deadline = Instant::now() + SOCKET_CONNECT_TIMEOUT;

    loop {
        match UnixStream::connect(path) {
            Ok(socket) => return Ok(socket),
            Err(connect_error) => {
                if let Some(status) = child.try_wait()? {
                    return Err(io::Error::other(format!(
                        "media engine exited before opening {}: {status}",
                        path.display()
                    )));
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        connect_error.kind(),
                        format!(
                            "media engine socket {} was not ready within {:?}: {connect_error}",
                            path.display(),
                            SOCKET_CONNECT_TIMEOUT
                        ),
                    ));
                }
                thread::sleep(SOCKET_CONNECT_RETRY);
            }
        }
    }
}

fn terminate_child(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }

    send_signal(child.id(), SIGTERM)?;
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(SHUTDOWN_POLL_INTERVAL);
    }

    send_signal(child.id(), SIGKILL)?;
    child.wait().map(|_| ())
}

fn send_signal(pid: u32, signal: i32) -> io::Result<()> {
    let pid = i32::try_from(pid).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("process id {pid} does not fit the camera's POSIX ABI"),
        )
    })?;

    // SAFETY: `pid` came from `Child::id`, the signal values are fixed POSIX
    // constants, and `kill` does not retain either argument after returning.
    let result = unsafe { kill(pid, signal) };
    if result == 0 {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    // ESRCH means the child exited between try_wait and kill; wait/reap below
    // still provides the authoritative process result.
    if error.raw_os_error() == Some(ESRCH) {
        return Ok(());
    }
    Err(error)
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_validated_camera_paths() {
        let config = MediaEngineConfig::default();

        assert_eq!(config.program, Path::new(DEFAULT_PROGRAM));
        assert_eq!(config.socket_path, Path::new(DEFAULT_SOCKET_PATH));
        assert_eq!(config.video_device, Path::new(DEFAULT_VIDEO_DEVICE));
        assert_eq!(config.audio_card, DEFAULT_AUDIO_CARD);
        assert_eq!(config.iq_directory, Path::new(DEFAULT_IQ_DIRECTORY));
        assert!(config.audio_enabled);
    }

    #[test]
    fn talkback_sink_assigns_contiguous_pcm_timestamps() {
        let (mut companion, daemon) = UnixStream::pair().expect("Unix pair should create");
        let handle = MediaEngineHandle {
            socket: Arc::new(Mutex::new(daemon)),
            next_speaker_timestamp_us: Arc::new(AtomicU64::new(0)),
        };
        let sink: &dyn crate::rtsp::TalkbackSink = &handle;

        sink.start_talkback().expect("talkback should start");
        sink.send_pcm(&[0; 320]).expect("first PCM should write");
        sink.send_pcm(&[0; 320]).expect("second PCM should write");

        assert_eq!(
            read_record(&mut companion)
                .expect("clear command should read")
                .expect("clear command should be present")
                .kind,
            RecordKind::SPEAKER_CLEAR
        );
        assert_eq!(
            read_record(&mut companion)
                .expect("first PCM command should read")
                .expect("first PCM command should be present")
                .timestamp_us,
            0
        );
        assert_eq!(
            read_record(&mut companion)
                .expect("second PCM command should read")
                .expect("second PCM command should be present")
                .timestamp_us,
            10_000
        );
    }

    #[test]
    fn command_handle_frames_speaker_controls_without_interleaving() {
        let (mut companion, daemon) = UnixStream::pair().expect("Unix pair should create");
        let handle = MediaEngineHandle {
            socket: Arc::new(Mutex::new(daemon)),
            next_speaker_timestamp_us: Arc::new(AtomicU64::new(0)),
        };

        handle
            .send_speaker_pcm(&[1, 2], 7)
            .expect("PCM command should write");
        handle
            .send_speaker_aac(&[3, 4], 9)
            .expect("AAC command should write");
        handle
            .set_speaker_volume(42)
            .expect("volume command should write");
        handle
            .set_night_mode(1)
            .expect("night-mode command should write");
        handle.clear_speaker().expect("clear command should write");

        assert_eq!(
            read_record(&mut companion)
                .expect("PCM command should read")
                .expect("PCM command should be present"),
            MediaRecord {
                kind: RecordKind::SPEAKER_PCM,
                codec: 0,
                keyframe: true,
                timestamp_us: 7,
                payload: vec![1, 2],
            }
        );
        assert_eq!(
            read_record(&mut companion)
                .expect("AAC command should read")
                .expect("AAC command should be present"),
            MediaRecord {
                kind: RecordKind::SPEAKER_PCM,
                codec: 0,
                keyframe: false,
                timestamp_us: 9,
                payload: vec![3, 4],
            }
        );
        assert_eq!(
            read_record(&mut companion)
                .expect("volume command should read")
                .expect("volume command should be present")
                .payload,
            42_u32.to_le_bytes().to_vec()
        );
        assert_eq!(
            read_record(&mut companion)
                .expect("night-mode command should read")
                .expect("night-mode command should be present")
                .payload,
            vec![1]
        );
        assert_eq!(
            read_record(&mut companion)
                .expect("clear command should read")
                .expect("clear command should be present")
                .kind,
            RecordKind::SPEAKER_CLEAR
        );
        assert!(handle.set_speaker_volume(101).is_err());
        assert!(handle.set_night_mode(3).is_err());
    }
}

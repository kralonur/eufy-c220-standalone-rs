//! Owner for the camera-local native media engine.
//!
//! The companion process owns the sensor/ISP/V4L2/VENC hardware and exposes
//! encoded records over a private Unix socket. This Rust boundary is based on
//! the reverse-engineered launch arguments and shutdown behavior in
//! `native/c220-media-engine.c`; the companion is part of our camera firmware,
//! not a vendor/P2P client.

use crate::media::{MediaRecord, RecordKind};
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
}

impl Default for MediaEngineConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::from(DEFAULT_PROGRAM),
            socket_path: PathBuf::from(DEFAULT_SOCKET_PATH),
            video_device: PathBuf::from(DEFAULT_VIDEO_DEVICE),
            audio_card: DEFAULT_AUDIO_CARD.to_string(),
            iq_directory: PathBuf::from(DEFAULT_IQ_DIRECTORY),
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
}

impl MediaEngine {
    /// Launch the companion and connect to its private socket.
    pub fn start(config: MediaEngineConfig) -> io::Result<Self> {
        let mut child = launch(&config)?;
        let socket = match connect_until_ready(&config.socket_path, &mut child) {
            Ok(socket) => socket,
            Err(error) => {
                let error_kind = error.kind();
                let message = format!("media engine failed to become ready: {error}");
                if let Err(cleanup_error) = terminate_child(&mut child) {
                    return Err(io::Error::new(
                        error_kind,
                        format!("{message}; failed to terminate child: {cleanup_error}"),
                    ));
                }
                return Err(io::Error::new(error_kind, message));
            }
        };

        Ok(Self { child, socket })
    }

    /// Read the next record emitted by the companion.
    pub fn read_record(&mut self) -> io::Result<Option<MediaRecord>> {
        MediaRecord::read_from(&mut self.socket)
    }

    /// Send one command record to the companion.
    pub fn write_record(&mut self, record: &MediaRecord) -> io::Result<()> {
        record.write_to(&mut self.socket)
    }

    /// Request the camera speaker to discard queued playback.
    pub fn clear_speaker(&mut self) -> io::Result<()> {
        self.write_record(&MediaRecord {
            kind: RecordKind::SPEAKER_CLEAR,
            codec: 0,
            keyframe: false,
            timestamp_us: 0,
            payload: Vec::new(),
        })
    }

    /// Stop the companion and reap it before returning.
    pub fn shutdown(&mut self) -> io::Result<()> {
        let socket_result = self.socket.shutdown(Shutdown::Both);
        let child_result = terminate_child(&mut self.child);

        match (socket_result, child_result) {
            (_, Err(error)) => Err(error),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

impl Drop for MediaEngine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn launch(config: &MediaEngineConfig) -> io::Result<Child> {
    Command::new(&config.program)
        .arg("--socket")
        .arg(&config.socket_path)
        .arg("--video-device")
        .arg(&config.video_device)
        .arg("--card")
        .arg(&config.audio_card)
        .arg("--iq-dir")
        .arg(&config.iq_directory)
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
    }
}

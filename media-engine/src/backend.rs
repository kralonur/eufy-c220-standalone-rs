//! What the engine does with daemon commands.
//!
//! The trait exists so the daemon protocol can be exercised without a camera:
//! [`MockBackend`] keeps the state the protocol exposes and logs every command.
//! The Rockchip backend that replaces it drives the real sensor, speaker and
//! microphone paths, but it appears on the wire in exactly the same way.

use crate::client::RecordWriter;
use crate::commands::{Command, MAX_VOLUME, NightModeTarget};
use crate::platform::Error;
use crate::platform::media_log;

/// The device side of the engine.
pub trait Backend {
    /// Handle one command from the daemon.
    ///
    /// The writer is the connection the command arrived on, so a backend can
    /// report state changes (night mode) back to the daemon.
    fn command(&mut self, command: Command<'_>, writer: &RecordWriter<'_>) -> Result<(), Error>;

    /// Current night state: `(night, chosen manually)`.
    ///
    /// It is sent to the daemon whenever a session starts.
    fn night_mode(&self) -> (bool, bool);

    /// Release everything the backend owns.
    fn shutdown(&mut self);
}

/// Hardware-free backend.
///
/// It models the state the protocol reports - night mode, and the volume merely
/// as a log line - which is enough for the daemon-facing behavior to be tested
/// on a host and, until the vendor bindings land, on the camera.
pub struct MockBackend {
    night: bool,
    target: NightModeTarget,
}

impl MockBackend {
    /// Create a backend in the state a freshly started engine is in: day mode,
    /// choosing for itself.
    pub const fn new() -> Self {
        Self {
            night: false,
            target: NightModeTarget::Auto,
        }
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for MockBackend {
    fn command(&mut self, command: Command<'_>, writer: &RecordWriter<'_>) -> Result<(), Error> {
        match command {
            Command::SpeakerClear => media_log!("media_speaker_clear=1"),
            Command::SpeakerVolume { volume } => {
                media_log!("media_speaker_volume={}/{}", volume, MAX_VOLUME)
            }
            Command::NightMode { target } => {
                self.target = target;
                // Auto leaves the current state alone; the sensor path decides,
                // and there is none in the mock.
                let night = match target {
                    NightModeTarget::Auto => self.night,
                    NightModeTarget::Night => true,
                    NightModeTarget::Day => false,
                };
                if night != self.night {
                    self.night = night;
                    media_log!("media_night_mode_switch={}", u8::from(night));
                    writer.send_night_mode(night, self.target != NightModeTarget::Auto)?;
                }
                media_log!("media_night_mode_set={}", target.name());
            }
            Command::SpeakerPcm { payload, .. } => {
                media_log!("media_speaker_pcm_bytes={}", payload.len())
            }
            Command::SpeakerAac { payload, .. } => {
                media_log!("media_speaker_aac_bytes={}", payload.len())
            }
        }
        Ok(())
    }

    fn night_mode(&self) -> (bool, bool) {
        (self.night, self.target != NightModeTarget::Auto)
    }

    fn shutdown(&mut self) {
        media_log!("media_mock_backend_stopped=1");
    }
}

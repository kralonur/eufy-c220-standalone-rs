//! Hardware-free backend.
//!
//! It models the state the protocol reports - night mode, and the volume merely
//! as a log line - which is enough for the daemon-facing behavior to be tested on
//! a host and, with `--mock-backend`, on the camera.

use super::NightState;
use crate::client::RecordSink;
use crate::commands::{Command, MAX_VOLUME};
use crate::platform::Error;
use crate::platform::media_log;

/// A backend that touches no hardware.
pub struct MockBackend {
    night: NightState,
}

impl MockBackend {
    /// Create a backend in the state a freshly started engine is in.
    pub const fn new() -> Self {
        Self {
            night: NightState::new(),
        }
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl super::Backend for MockBackend {
    fn command(&mut self, command: Command<'_>, sink: &RecordSink) -> Result<(), Error> {
        match command {
            Command::SpeakerClear => media_log!("media_speaker_clear=1"),
            Command::SpeakerVolume { volume } => {
                media_log!("media_speaker_volume={}/{}", volume, MAX_VOLUME)
            }
            Command::NightMode { target } => {
                if let Some(night) = self.night.apply(target) {
                    media_log!("media_night_mode_switch={}", u8::from(night));
                    let (night, manual) = self.night.reported();
                    sink.send_night_mode(night, manual)?;
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
        self.night.reported()
    }

    fn shutdown(&mut self) {
        media_log!("media_mock_backend_stopped=1");
    }
}

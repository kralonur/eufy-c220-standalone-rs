//! Commands the daemon sends to the engine.
//!
//! The reference `serve_client` both bounds and interprets these records; the
//! rules live here so they can be tested without a socket. A record that breaks
//! a rule ends the session, which is what the reference implementation does when
//! it leaves its read loop.

use c220_media_protocol::RecordKind;

/// Largest speaker payload in the ABI: the reference engine's `MAX_PCM_PAYLOAD`.
pub const MAX_PCM_PAYLOAD: usize = 64 * 1024;

/// Largest AAC payload in the ABI: the reference engine's `MAX_AAC_PAYLOAD`.
pub const MAX_AAC_PAYLOAD: usize = 48 * 1024;

/// Volume ceiling of the vendor AO device: the reference engine's
/// `MAX_AO_VOLUME`.
pub const MAX_VOLUME: u32 = 100;

/// What the daemon asked the engine to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    /// Discard queued speaker playback.
    SpeakerClear,
    /// Set the speaker volume, already known to be within [`MAX_VOLUME`].
    SpeakerVolume { volume: u32 },
    /// Change the night-mode target.
    NightMode { target: NightModeTarget },
    /// Play signed 16-bit mono PCM from the daemon.
    SpeakerPcm {
        payload: &'a [u8],
        timestamp_us: u64,
    },
    /// Play one AAC access unit from the daemon.
    SpeakerAac {
        payload: &'a [u8],
        timestamp_us: u64,
    },
}

/// Night-mode request from the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NightModeTarget {
    /// Let the engine decide from the image statistics.
    Auto,
    /// Force night mode.
    Night,
    /// Force day mode.
    Day,
}

impl NightModeTarget {
    /// The ABI value the daemon sends.
    pub const fn from_abi(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Auto),
            1 => Some(Self::Night),
            2 => Some(Self::Day),
            _ => None,
        }
    }

    /// The name used in diagnostics, matching the reference engine's logs.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Night => "night",
            Self::Day => "day",
        }
    }
}

/// A record that breaks the command ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// The payload is longer than the kind allows.
    Length { limit: usize },
    /// The payload is shorter than the kind requires.
    Empty,
}

/// Largest payload the daemon may send for this record kind.
///
/// A longer payload ends the session, which is what the reference engine does
/// when it leaves its read loop.
pub fn payload_limit(kind: RecordKind, keyframe: bool) -> usize {
    match kind {
        // The PCM record carries AAC unless the keyframe flag marks raw PCM.
        RecordKind::SpeakerPcm if !keyframe => MAX_AAC_PAYLOAD,
        // The volume command is exactly one little-endian word.
        RecordKind::SpeakerVolume => core::mem::size_of::<u32>(),
        // Everything else the daemon can send is bounded by the PCM limit,
        // including kinds this engine does not act on.
        _ => MAX_PCM_PAYLOAD,
    }
}

/// Interpret a record that has already been read from the socket.
///
/// `Ok(None)` means the record is well-formed but asks for nothing: an unknown
/// kind, or a volume outside the device's range, which the reference engine also
/// ignores silently.
pub fn interpret<'a>(
    kind: RecordKind,
    keyframe: bool,
    payload: &'a [u8],
    timestamp_us: u64,
) -> Result<Option<Command<'a>>, CommandError> {
    match kind {
        RecordKind::SpeakerClear => Ok(Some(Command::SpeakerClear)),
        RecordKind::SpeakerVolume => {
            if payload.len() != core::mem::size_of::<u32>() {
                return Err(if payload.is_empty() {
                    CommandError::Empty
                } else {
                    CommandError::Length {
                        limit: core::mem::size_of::<u32>(),
                    }
                });
            }
            let volume = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            if volume > MAX_VOLUME {
                return Ok(None);
            }
            Ok(Some(Command::SpeakerVolume { volume }))
        }
        RecordKind::NightMode => {
            match NightModeTarget::from_abi(payload.first().copied().unwrap_or(0)) {
                // The reference engine reads only the first byte and ignores the
                // rest of the payload.
                Some(target) => Ok(Some(Command::NightMode { target })),
                None => Ok(None),
            }
        }
        RecordKind::SpeakerPcm => {
            if payload.is_empty() {
                return Err(CommandError::Empty);
            }
            if keyframe {
                Ok(Some(Command::SpeakerPcm {
                    payload,
                    timestamp_us,
                }))
            } else {
                Ok(Some(Command::SpeakerAac {
                    payload,
                    timestamp_us,
                }))
            }
        }
        // Video, microphone and unknown kinds are not commands this engine
        // acts on; the reference engine drains them and carries on.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_follow_the_reference_engine() {
        assert_eq!(payload_limit(RecordKind::SpeakerPcm, true), MAX_PCM_PAYLOAD);
        assert_eq!(
            payload_limit(RecordKind::SpeakerPcm, false),
            MAX_AAC_PAYLOAD
        );
        assert_eq!(payload_limit(RecordKind::SpeakerVolume, false), 4);
        assert_eq!(
            payload_limit(RecordKind::SpeakerClear, false),
            MAX_PCM_PAYLOAD
        );
        assert_eq!(payload_limit(RecordKind::NightMode, false), MAX_PCM_PAYLOAD);
        assert_eq!(payload_limit(RecordKind::Video, true), MAX_PCM_PAYLOAD);
        assert_eq!(
            payload_limit(RecordKind::Unknown(200), false),
            MAX_PCM_PAYLOAD
        );
    }

    #[test]
    fn interprets_speaker_commands() {
        assert_eq!(
            interpret(RecordKind::SPEAKER_CLEAR, false, &[], 0).expect("clear"),
            Some(Command::SpeakerClear)
        );
        assert_eq!(
            interpret(RecordKind::SPEAKER_VOLUME, false, &[42, 0, 0, 0], 7).expect("volume"),
            Some(Command::SpeakerVolume { volume: 42 })
        );
        assert_eq!(
            interpret(RecordKind::SPEAKER_PCM, true, &[1, 2], 9).expect("pcm"),
            Some(Command::SpeakerPcm {
                payload: &[1, 2],
                timestamp_us: 9
            })
        );
        assert_eq!(
            interpret(RecordKind::SPEAKER_PCM, false, &[3, 4], 11).expect("aac"),
            Some(Command::SpeakerAac {
                payload: &[3, 4],
                timestamp_us: 11
            })
        );
    }

    #[test]
    fn interprets_every_night_mode_target() {
        for (value, target) in [
            (0, NightModeTarget::Auto),
            (1, NightModeTarget::Night),
            (2, NightModeTarget::Day),
        ] {
            assert_eq!(
                interpret(RecordKind::NIGHT_MODE, false, &[value], 0).expect("night mode"),
                Some(Command::NightMode { target })
            );
            assert_eq!(
                target.name(),
                match value {
                    0 => "auto",
                    1 => "night",
                    _ => "day",
                }
            );
        }
    }

    #[test]
    fn ignores_records_that_ask_for_nothing() {
        // A volume above the device's range is ignored, not fatal.
        assert_eq!(
            interpret(RecordKind::SPEAKER_VOLUME, false, &[101, 0, 0, 0], 0).expect("volume"),
            None
        );
        // Unknown night-mode values, unknown kinds and drained streams are
        // ignored.
        assert_eq!(
            interpret(RecordKind::NIGHT_MODE, false, &[9], 0).expect("night mode"),
            None
        );
        assert_eq!(
            interpret(RecordKind::Unknown(200), false, &[1], 0).expect("unknown"),
            None
        );
        assert_eq!(
            interpret(RecordKind::VIDEO, true, &[1, 2, 3], 0).expect("video"),
            None
        );
    }

    #[test]
    fn rejects_malformed_payloads() {
        assert_eq!(
            interpret(RecordKind::SPEAKER_VOLUME, false, &[1, 0, 0], 0),
            Err(CommandError::Length { limit: 4 })
        );
        assert_eq!(
            interpret(RecordKind::SPEAKER_VOLUME, false, &[], 0),
            Err(CommandError::Empty)
        );
        assert_eq!(
            interpret(RecordKind::SPEAKER_PCM, true, &[], 0),
            Err(CommandError::Empty)
        );
        assert_eq!(
            interpret(RecordKind::SPEAKER_PCM, false, &[], 0),
            Err(CommandError::Empty)
        );
    }
}

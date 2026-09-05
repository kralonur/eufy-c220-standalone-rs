//! Command-line contract of the media engine process.
//!
//! The option set and defaults mirror the reference `native/c220-media-engine.c`
//! so the firmware can launch either binary with the same arguments. Two
//! deliberate differences are documented on [`parse_arguments`].

use alloc::string::{String, ToString};

/// Default private socket shared by the firmware and the media engine.
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/c220-media.sock";

/// Validated 2K self-path capture node from the camera hardware evidence.
pub const DEFAULT_VIDEO_DEVICE: &str = "/dev/video12";

/// Audio card used for microphone capture and speaker playback.
pub const DEFAULT_AUDIO_CARD: &str = "hw:0,0";

/// Directory holding the camera's preserved AIQ calibration files.
pub const DEFAULT_IQ_DIRECTORY: &str = "/home";

/// Frame rate the 2K sensor/ISP mode actually delivers (measured ~15.17 fps).
pub const DEFAULT_VIDEO_FPS: i32 = 15;

/// Main-stream constant-bitrate budget in kilobits per second.
pub const DEFAULT_VIDEO_BITRATE_KBPS: i32 = 2048;

/// Runtime configuration of the media engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    /// Private Unix socket the engine creates and listens on.
    pub socket_path: String,
    /// V4L2 capture node that provides the 2K NV12 frames.
    pub video_device: String,
    /// Audio card used by the AI/AO devices.
    pub audio_card: String,
    /// Directory containing the AIQ calibration/configuration files.
    pub iq_directory: String,
    /// Whether the microphone capture path is opened.
    pub audio_capture_enabled: bool,
    /// Run without touching the hardware.
    ///
    /// The camera's device paths are the vendors' libraries, whose constructors
    /// fault where the hardware is absent, so this is what makes the protocol
    /// testable under an emulator - and what the reference engine's diagnostic
    /// switches are for too.
    pub mock_backend: bool,
    /// Main-stream frame rate.
    pub video_fps: i32,
    /// Main-stream constant-bitrate budget in kilobits per second.
    pub video_bitrate_kbps: i32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            socket_path: DEFAULT_SOCKET_PATH.to_string(),
            video_device: DEFAULT_VIDEO_DEVICE.to_string(),
            audio_card: DEFAULT_AUDIO_CARD.to_string(),
            iq_directory: DEFAULT_IQ_DIRECTORY.to_string(),
            audio_capture_enabled: true,
            mock_backend: false,
            video_fps: DEFAULT_VIDEO_FPS,
            video_bitrate_kbps: DEFAULT_VIDEO_BITRATE_KBPS,
        }
    }
}

/// A rejected command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// An option that takes a value was the last argument.
    MissingValue(&'static str),
    /// A numeric option was not a valid number, or was outside its range.
    InvalidNumber(&'static str),
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingValue(option) => write!(formatter, "{option} requires a value"),
            Self::InvalidNumber(option) => write!(formatter, "{option} requires a valid number"),
        }
    }
}

impl core::error::Error for ConfigError {}

/// Parse engine arguments, starting from [`EngineConfig::default`].
///
/// Options are processed in order, so a later option overrides an earlier one.
///
/// Two intentional improvements over the reference implementation, which reads
/// values with `atoi` and never checks for a missing argument:
///
/// * a missing or non-numeric value is an error instead of silently becoming
///   `0` or reading past `argv`; and
/// * `--fps` and `--bitrate` reject values below `1` (the reference casts them
///   to unsigned for the encoder attributes, where `0` or a negative number is
///   meaningless).
pub fn parse_arguments<'a>(
    arguments: impl IntoIterator<Item = &'a str>,
) -> Result<EngineConfig, ConfigError> {
    let mut config = EngineConfig::default();
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument {
            "--socket" => config.socket_path = value(&mut arguments, "--socket")?.to_string(),
            "--video-device" => {
                config.video_device = value(&mut arguments, "--video-device")?.to_string();
            }
            "--card" => config.audio_card = value(&mut arguments, "--card")?.to_string(),
            "--iq-dir" => config.iq_directory = value(&mut arguments, "--iq-dir")?.to_string(),
            "--no-audio" => config.audio_capture_enabled = false,
            "--mock-backend" => config.mock_backend = true,
            "--fps" => {
                config.video_fps = positive_number(&mut arguments, "--fps")?;
            }
            "--bitrate" => {
                config.video_bitrate_kbps = positive_number(&mut arguments, "--bitrate")?;
            }
            // Unknown options are ignored, as in the reference engine, so a
            // firmware that passes a future option still starts.
            _ => {}
        }
    }
    Ok(config)
}

fn value<'a>(
    arguments: &mut impl Iterator<Item = &'a str>,
    option: &'static str,
) -> Result<&'a str, ConfigError> {
    arguments.next().ok_or(ConfigError::MissingValue(option))
}

fn number<'a>(
    arguments: &mut impl Iterator<Item = &'a str>,
    option: &'static str,
) -> Result<i32, ConfigError> {
    value(arguments, option)?
        .parse::<i32>()
        .map_err(|_| ConfigError::InvalidNumber(option))
}

fn positive_number<'a>(
    arguments: &mut impl Iterator<Item = &'a str>,
    option: &'static str,
) -> Result<i32, ConfigError> {
    match number(arguments, option)? {
        value if value >= 1 => Ok(value),
        _ => Err(ConfigError::InvalidNumber(option)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_reference_engine() {
        let config = parse_arguments([""; 0]).expect("empty command line is valid");

        assert_eq!(config, EngineConfig::default());
        assert_eq!(config.socket_path, "/tmp/c220-media.sock");
        assert_eq!(config.video_device, "/dev/video12");
        assert_eq!(config.audio_card, "hw:0,0");
        assert_eq!(config.iq_directory, "/home");
        assert!(config.audio_capture_enabled);
        assert!(!config.mock_backend);
        assert_eq!(config.video_fps, 15);
        assert_eq!(config.video_bitrate_kbps, 2048);
    }

    #[test]
    fn parses_the_arguments_the_firmware_passes() {
        let config = parse_arguments([
            "--socket",
            "/tmp/other.sock",
            "--video-device",
            "/dev/video0",
            "--card",
            "hw:1,0",
            "--iq-dir",
            "/tmp/iq",
            "--no-audio",
        ])
        .expect("firmware arguments are valid");

        assert_eq!(config.socket_path, "/tmp/other.sock");
        assert_eq!(config.video_device, "/dev/video0");
        assert_eq!(config.audio_card, "hw:1,0");
        assert_eq!(config.iq_directory, "/tmp/iq");
        assert!(!config.audio_capture_enabled);
    }

    #[test]
    fn rejects_missing_and_invalid_values() {
        assert_eq!(
            parse_arguments(["--socket"]),
            Err(ConfigError::MissingValue("--socket"))
        );
        assert_eq!(
            parse_arguments(["--video-device"]),
            Err(ConfigError::MissingValue("--video-device"))
        );
        assert_eq!(
            parse_arguments(["--fps", "0"]),
            Err(ConfigError::InvalidNumber("--fps"))
        );
        assert_eq!(
            parse_arguments(["--fps", "fast"]),
            Err(ConfigError::InvalidNumber("--fps"))
        );
        assert_eq!(
            parse_arguments(["--bitrate", "-1"]),
            Err(ConfigError::InvalidNumber("--bitrate"))
        );
    }

    #[test]
    fn ignores_unknown_options() {
        let config = parse_arguments(["--future-option", "value", "--fps", "20"])
            .expect("unknown options are ignored like the reference engine");

        assert_eq!(config.video_fps, 20);
        assert_eq!(config.video_bitrate_kbps, DEFAULT_VIDEO_BITRATE_KBPS);
    }
}

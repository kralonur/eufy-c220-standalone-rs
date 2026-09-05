//! The speaker path: daemon talkback out of the camera's playback device.
//!
//! Ported from the reference engine's `init_speaker`, `send_pcm_frame`,
//! `speaker_send_aac`, `speaker_send_pcm16` and `speaker_clear`. The daemon sends
//! either raw PCM or AAC access units; AAC goes through the FDK decoder, and both
//! end up as 16-bit mono blocks on the AO device, which resamples 16 kHz talkback
//! to the card's 8 kHz.
//!
//! Two behaviors are worth knowing, because they look like oversights and are
//! deliberate:
//!
//! * The amplifier pin is driven high on the first block of a burst and low again
//!   when the daemon clears playback, which is how the reference engine keeps the
//!   speaker quiet between bursts.
//! * A malformed or undecodable block is logged and dropped, and the session
//!   continues: the daemon is not told, exactly as the reference engine does it.

use super::{Backend, NightState};
use crate::client::RecordWriter;
use crate::commands::{Command, MAX_PCM_PAYLOAD, MAX_VOLUME};
use crate::gpio::OutputPin;
use crate::platform::{Error, media_log};
use crate::vendor::fdk::{Decoded, Decoder};
use crate::vendor::mpi::{
    AUDIO_BIT_WIDTH_16, AUDIO_CHN_MODE_DEFAULT, AUDIO_LOOPBACK_NONE, AUDIO_SAMPLE_RATE_8000,
    AUDIO_SAMPLE_RATE_16000, AUDIO_SOUND_MODE_MONO, AUDIO_SOUND_MODE_STEREO,
    AUDIO_TRACK_OUT_STEREO, MAX_AUDIO_CARD_NAME,
};
use crate::vendor::mpi::{
    AudioAttributes, AudioOutput, AudioOutputChannelParams, ExternalBlock, SoundCard,
};

/// The amplifier enable pin, from the reference engine.
const SPEAKER_GPIO: u32 = 102;

/// Volume the device starts at; the daemon can lower it but not raise it past
/// [`MAX_VOLUME`].
const DEFAULT_VOLUME: i32 = 100;

/// Samples the decoder may produce in one call.
const PCM_SAMPLES: usize = 4096;

/// Frames the device is asked to hold (reference value).
const DEVICE_FRAMES: u32 = 4;

/// Samples per device frame (reference value).
const DEVICE_SAMPLES_PER_FRAME: u32 = 1024;

/// Channels the device is configured with (reference value).
const DEVICE_CHANNELS: u32 = 2;

/// Timeout for one playback block, in milliseconds (reference value).
const SEND_TIMEOUT_MS: i32 = 1000;

/// Decode attempts per access unit before giving up (reference value).
const MAX_DECODE_ATTEMPTS: usize = 64;

/// The camera's playback device.
struct Speaker {
    amplifier: OutputPin,
    decoder: Decoder,
    output: AudioOutput,
    sequence: u32,
}

impl Speaker {
    /// Open the card, the resampler and the AAC decoder.
    fn open(card: &str) -> Result<Self, Error> {
        // The amplifier stays off until there is something to play.
        let amplifier = OutputPin::open(SPEAKER_GPIO)?;

        let mut attributes = AudioAttributes {
            card: SoundCard {
                channels: DEVICE_CHANNELS,
                sample_rate: AUDIO_SAMPLE_RATE_8000 as u32,
                bit_width: AUDIO_BIT_WIDTH_16,
            },
            sample_rate: AUDIO_SAMPLE_RATE_8000,
            bit_width: AUDIO_BIT_WIDTH_16,
            sound_mode: AUDIO_SOUND_MODE_MONO,
            extended_flags: 0,
            frame_count: DEVICE_FRAMES,
            points_per_frame: DEVICE_SAMPLES_PER_FRAME,
            channel_count: DEVICE_CHANNELS,
            card_name: [0; MAX_AUDIO_CARD_NAME],
        };
        copy_card_name(&mut attributes.card_name, card);

        let params = AudioOutputChannelParams {
            mode: AUDIO_CHN_MODE_DEFAULT,
            loopback_mode: AUDIO_LOOPBACK_NONE,
        };
        let output = AudioOutput::open(
            0,
            0,
            &attributes,
            &params,
            AUDIO_SAMPLE_RATE_16000,
            DEFAULT_VOLUME,
            AUDIO_TRACK_OUT_STEREO,
        )?;
        let decoder = Decoder::open()?;

        media_log!(
            "media_speaker_ready=aac+pcm/16000-to-8000/mono volume={}/{} gpio={} card={}",
            DEFAULT_VOLUME,
            MAX_VOLUME,
            SPEAKER_GPIO,
            card
        );
        Ok(Self {
            amplifier,
            decoder,
            output,
            sequence: 0,
        })
    }

    /// Set the playback volume.
    fn set_volume(&mut self, volume: u32) -> Result<(), Error> {
        self.output.set_volume(volume as i32)?;
        media_log!("media_speaker_volume={}/{}", volume, MAX_VOLUME);
        Ok(())
    }

    /// Discard queued playback and quiet the amplifier.
    fn clear(&mut self) {
        if self.amplifier.is_high()
            && let Err(error) = self.amplifier.set(false)
        {
            media_log!("media_speaker_gpio_error={error}");
        }
        if let Err(error) = self.output.clear() {
            media_log!("media_speaker_clear_error={error}");
        }
    }

    /// Play one block of raw PCM from the daemon.
    fn play_pcm(&mut self, payload: &[u8], timestamp_us: u64) {
        if payload.is_empty() || !payload.len().is_multiple_of(2) {
            // The reference engine logs the length and returns without playing.
            media_log!("media_pcm_length_error={}", payload.len());
            return;
        }
        if payload.len() > MAX_PCM_PAYLOAD {
            media_log!("media_pcm_payload_error={}", payload.len());
            return;
        }
        // The reference engine applies an attenuation shift to every sample here;
        // the shift is zero, so the samples are played as they arrive.
        self.play_block(payload, 1, timestamp_us);
    }

    /// Decode one AAC access unit and play whatever frames it yields.
    fn play_aac(&mut self, payload: &[u8], timestamp_us: u64) {
        let mut remaining = payload;
        let mut pcm = [0_i16; PCM_SAMPLES];

        for _ in 0..MAX_DECODE_ATTEMPTS {
            if !remaining.is_empty() {
                match self.decoder.fill(remaining) {
                    Ok(unconsumed) => {
                        // The decoder reports what it did not consume; the
                        // reference engine never advances its pointer and feeds
                        // the buffer from the start again, which is only harmless
                        // while nothing is ever left over.
                        remaining = &remaining[remaining.len().saturating_sub(unconsumed)..];
                    }
                    Err(error) => {
                        media_log!("media_aac_fill_error={error}");
                        return;
                    }
                }
            }

            match self.decoder.decode(&mut pcm) {
                Ok(Decoded::Frame) => {}
                Ok(Decoded::NeedMoreInput) => break,
                Err(error) => {
                    media_log!("media_aac_decode_error={error}");
                    return;
                }
            }

            let Some(info) = self.decoder.stream_info() else {
                media_log!("media_aac_stream_info_error=1");
                return;
            };
            if info.sample_rate == 0 || info.channels <= 0 || info.frame_size <= 0 {
                media_log!(
                    "media_aac_stream_info_error=1 rate={} channels={} frame={}",
                    info.sample_rate,
                    info.channels,
                    info.frame_size
                );
                return;
            }
            let sample_count = (info.frame_size as usize).saturating_mul(info.channels as usize);
            if sample_count > pcm.len() {
                media_log!("media_aac_frame_size_error={sample_count}");
                return;
            }

            // SAFETY: `sample_count` samples were just written into `pcm`, so the
            // byte view covers only initialised memory.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    pcm.as_ptr() as *const u8,
                    sample_count * core::mem::size_of::<i16>(),
                )
            };
            self.play_block(bytes, info.channels as u32, timestamp_us);
        }
    }

    /// Hand one block of PCM bytes to the device, enabling the amplifier first.
    fn play_block(&mut self, samples: &[u8], channels: u32, timestamp_us: u64) {
        if let Err(error) = self.amplifier.set_if_low() {
            media_log!("media_speaker_gpio_error={error}");
            return;
        }

        // The device expects the block length for its channel count: the byte
        // count for mono, and the byte count divided by the channels otherwise.
        let length = match channels {
            1 => samples.len() as u32,
            2 => (samples.len() >> 1) as u32,
            4 => (samples.len() >> 2) as u32,
            other => {
                media_log!("media_unsupported_channels={other}");
                return;
            }
        };
        let sound_mode = if channels == 1 {
            AUDIO_SOUND_MODE_MONO
        } else {
            AUDIO_SOUND_MODE_STEREO
        };

        let block = match ExternalBlock::new(samples) {
            Ok(block) => block,
            Err(error) => {
                media_log!("media_create_pcm_mb_error={error}");
                return;
            }
        };
        if let Err(error) = self.output.play(
            block.block(),
            length,
            sound_mode,
            timestamp_us,
            self.sequence,
            SEND_TIMEOUT_MS,
        ) {
            media_log!("media_ao_send_frame_error={error}");
            return;
        }
        self.sequence = self.sequence.wrapping_add(1);
        // The block is released when it goes out of scope, which is after the
        // device has taken the samples, as in the reference engine.
    }
}

/// Copy an ALSA card name into the fixed-size field, truncating like the
/// reference engine's `snprintf` and leaving it NUL-terminated.
fn copy_card_name(field: &mut [u8; MAX_AUDIO_CARD_NAME], card: &str) {
    let name = card.as_bytes();
    let taken = name.len().min(MAX_AUDIO_CARD_NAME - 1);
    field[..taken].copy_from_slice(&name[..taken]);
    field[taken] = 0;
}

/// The camera's playback backend.
///
/// It owns the speaker and keeps the night state the daemon is told about until
/// the image path lands; `--mock-backend` selects the hardware-free backend
/// instead, which is what the emulation tests use.
pub struct SpeakerBackend {
    card: alloc::string::String,
    speaker: Option<Speaker>,
    night: NightState,
}

impl SpeakerBackend {
    /// Create a backend for the given audio card.
    pub fn new(card: &str) -> Self {
        Self {
            card: alloc::string::String::from(card),
            speaker: None,
            night: NightState::new(),
        }
    }
}

impl Backend for SpeakerBackend {
    fn start(&mut self) -> Result<(), Error> {
        match Speaker::open(&self.card) {
            Ok(speaker) => {
                self.speaker = Some(speaker);
                Ok(())
            }
            Err(error) => {
                media_log!("media_speaker_error={error}");
                Err(error)
            }
        }
    }

    fn command(&mut self, command: Command<'_>, writer: &RecordWriter<'_>) -> Result<(), Error> {
        match command {
            Command::SpeakerClear => match self.speaker.as_mut() {
                Some(speaker) => {
                    speaker.clear();
                    media_log!("media_speaker_clear=1");
                }
                None => media_log!("media_speaker_unavailable=1"),
            },
            Command::SpeakerVolume { volume } => match self.speaker.as_mut() {
                Some(speaker) => speaker.set_volume(volume)?,
                None => media_log!("media_speaker_unavailable=1"),
            },
            Command::SpeakerPcm {
                payload,
                timestamp_us,
            } => match self.speaker.as_mut() {
                Some(speaker) => speaker.play_pcm(payload, timestamp_us),
                None => media_log!("media_speaker_unavailable=1"),
            },
            Command::SpeakerAac {
                payload,
                timestamp_us,
            } => match self.speaker.as_mut() {
                Some(speaker) => speaker.play_aac(payload, timestamp_us),
                None => media_log!("media_speaker_unavailable=1"),
            },
            Command::NightMode { target } => {
                if let Some(night) = self.night.apply(target) {
                    media_log!("media_night_mode_switch={}", u8::from(night));
                    let (night, manual) = self.night.reported();
                    writer.send_night_mode(night, manual)?;
                }
                media_log!("media_night_mode_set={}", target.name());
            }
        }
        Ok(())
    }

    fn night_mode(&self) -> (bool, bool) {
        self.night.reported()
    }

    fn shutdown(&mut self) {
        // Dropping the speaker clears the queue, disables the resampler, the
        // channel and the device, and drives the amplifier low.
        self.speaker = None;
        media_log!("media_speaker_stopped=1");
    }
}

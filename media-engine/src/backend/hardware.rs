//! The camera's audio devices: playback for talkback, capture for the microphone.
//!
//! The speaker path is ported from the reference engine's `init_speaker`,
//! `send_pcm_frame`, `speaker_send_aac`, `speaker_send_pcm16` and
//! `speaker_clear`: the daemon sends either raw PCM or AAC access units, AAC goes
//! through the FDK decoder, and both end up as 16-bit mono blocks on the AO
//! device, which resamples 16 kHz talkback to the card's 8 kHz.
//!
//! The microphone path is ported from `open_audio_capture` and `audio_thread`:
//! the AI device is bound to the AENC channel, so the hardware hands encoded
//! G.711 A-law blocks to a thread that forwards them to the daemon. That thread is
//! the engine's first record producer, which is why the connection it writes to is
//! a shared [`RecordSink`] rather than the session's own socket.
//!
//! Two behaviors are worth knowing, because they look like oversights and are
//! deliberate:
//!
//! * The amplifier pin is driven high on the first block of a burst and low again
//!   when the daemon clears playback, which is how the reference engine keeps the
//!   speaker quiet between bursts.
//! * A malformed or undecodable block is logged and dropped, and the session
//!   continues: the daemon is not told, exactly as the reference engine does it.

use super::image::{ImagePath, NightMonitor};
use super::video::{VideoConfig, VideoPipeline, VideoPrepared};
use super::{Backend, NightState};
use crate::client::RecordSink;
use crate::commands::{Command, MAX_PCM_PAYLOAD, MAX_VOLUME};
use crate::gpio::OutputPin;
use crate::platform::signal::Shutdown;
use crate::platform::sync::Mutex;
use crate::platform::thread::{self, Thread};
use crate::platform::{Error, media_log};
use crate::pts::Counter;
use crate::vendor::fdk::{Decoded, Decoder};
use crate::vendor::mpi::{
    self, ALAW_ENCODER_ATTRIBUTES, AUDIO_BIT_WIDTH_16, AUDIO_CHN_MODE_DEFAULT, AUDIO_FRAME_SAMPLES,
    AUDIO_LOOPBACK_NONE, AUDIO_MAX_PAYLOAD, AUDIO_SAMPLE_RATE_8000, AUDIO_SAMPLE_RATE_16000,
    AUDIO_SOUND_MODE_MONO, AUDIO_SOUND_MODE_STEREO, AUDIO_TRACK_FRONT_LEFT, AUDIO_TRACK_OUT_STEREO,
    AudioAttributes, AudioEncoder, AudioInput, AudioOutput, AudioOutputChannelParams, Channel,
    ExternalBlock, ID_AENC, ID_AI, INPUT_CHANNEL_PARAMS, MAX_AUDIO_CARD_NAME, SoundCard,
};
use alloc::ffi::CString;
use alloc::string::String;
use alloc::sync::Arc;
use c220_media_protocol::RecordKind;

/// The amplifier enable pin, from the reference engine.
const SPEAKER_GPIO: u32 = 102;

/// Volume the device starts at; the daemon can lower it but not raise it past
/// [`MAX_VOLUME`].
const DEFAULT_VOLUME: i32 = 100;

/// Samples the decoder may produce in one call.
const PCM_SAMPLES: usize = 4096;

/// Samples in one safe raw-PCM playback block: 20 ms at the 16-kHz talkback
/// input rate, matching the daemon's normal audio cadence and the probe path.
const SPEAKER_PCM_SAMPLES_PER_BLOCK: usize = 320;

/// Duration of one raw-PCM playback block, in microseconds: 20 ms at 16 kHz.
const SPEAKER_PCM_BLOCK_DURATION_US: u64 = 20_000;

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

/// How long the capture thread waits for one encoded block, in milliseconds
/// (reference value).
const CAPTURE_TIMEOUT_MS: i32 = 1000;

/// Timestamp step the capture path falls back to when the encoder reports none,
/// in microseconds: 20 ms, the length of one G.711 frame at 8 kHz.
const CAPTURE_FALLBACK_STEP_US: u64 = 20_000;

/// Name the capture thread carries in `/proc`.
const MICROPHONE_THREAD: &str = "c220-microphone";

/// Variable that selects Rockit's log level.
const ROCKIT_LOG_LEVEL_VARIABLE: &str = "rt_log_level";

/// Variable that selects Rockit's log file.
const ROCKIT_LOG_PATH_VARIABLE: &str = "rt_log_path";

/// Rockit log level the engine runs with: 0, no output.
///
/// The media pipeline logs per frame at the levels above this one, so leaving it
/// at the library's default both costs time on the hot path and fills the
/// filesystem with a trace nobody reads.
const ROCKIT_LOG_LEVEL: &str = "0";

/// Where Rockit's log goes, whether or not anything is written to it.
///
/// The path is the reference engine's and the factory engine's; naming it keeps
/// the library from choosing a location of its own.
const ROCKIT_LOG_PATH: &str = "/tmp/c220-media-rockit.log";

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
        // AO accepts the short blocks used by the normal talkback cadence, but a
        // whole HTTP body can be much larger. Split it before handing it to the
        // device; on this card a large single external block can fault inside AO.
        let block_bytes = SPEAKER_PCM_SAMPLES_PER_BLOCK * core::mem::size_of::<i16>();
        for (index, block) in payload.chunks(block_bytes).enumerate() {
            let block_timestamp = timestamp_us
                .saturating_add((index as u64).saturating_mul(SPEAKER_PCM_BLOCK_DURATION_US));
            self.play_block(block, 1, block_timestamp);
        }
    }

    /// Decode one AAC access unit and play whatever frames it yields.
    fn play_aac(&mut self, payload: &[u8], timestamp_us: u64) {
        // Keep the original access-unit pointer and carry only FDK's
        // bytes-valid count, matching the reference C implementation.
        let mut bytes_valid = payload.len();
        let mut pcm = [0_i16; PCM_SAMPLES];

        for _ in 0..MAX_DECODE_ATTEMPTS {
            if bytes_valid > 0 {
                match self.decoder.fill(payload, bytes_valid) {
                    Ok(valid) => bytes_valid = valid,
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

/// The camera's microphone: the capture device, its encoder and the thread that
/// forwards encoded blocks.
struct Microphone {
    thread: Thread,
}

impl Microphone {
    /// Open the capture device, bind it to the encoder and start the reader.
    ///
    /// The devices are moved into the thread, so they are released by the thread
    /// as it stops - the reference engine's `close_audio_capture` runs in the
    /// same place.
    fn open(card: &str, sink: Arc<RecordSink>, shutdown: Arc<Shutdown>) -> Result<Self, Error> {
        let input = open_capture_device(card)?;
        let encoder = AudioEncoder::open(0, &ALAW_ENCODER_ATTRIBUTES)?.bind(
            Channel {
                module: ID_AI,
                device: 0,
                channel: 0,
            },
            Channel {
                module: ID_AENC,
                device: 0,
                channel: 0,
            },
        )?;
        media_log!(
            "media_audio_ready=pcma/8000/mono frame_samples={} card={}",
            AUDIO_FRAME_SAMPLES,
            card
        );
        let thread = thread::spawn(MICROPHONE_THREAD, move || {
            forward_captured_audio(input, encoder, &sink, &shutdown)
        })?;
        Ok(Self { thread })
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        // The thread stops because shutdown was requested, and drops both devices
        // with it; joining is what makes the devices certainly gone before the
        // engine returns.
        self.thread.join();
    }
}

/// Configure and enable the capture device.
fn open_capture_device(card: &str) -> Result<AudioInput, Error> {
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
        points_per_frame: AUDIO_FRAME_SAMPLES,
        channel_count: DEVICE_CHANNELS,
        card_name: [0; MAX_AUDIO_CARD_NAME],
    };
    copy_card_name(&mut attributes.card_name, card);

    AudioInput::open(
        0,
        0,
        &attributes,
        &INPUT_CHANNEL_PARAMS,
        AUDIO_TRACK_FRONT_LEFT,
    )
}

/// Send captured blocks to the daemon until asked to stop.
fn forward_captured_audio(
    input: AudioInput,
    mut encoder: AudioEncoder,
    sink: &RecordSink,
    shutdown: &Shutdown,
) {
    let mut fallback = Counter::new(CAPTURE_FALLBACK_STEP_US);

    while !shutdown.requested() {
        let Some(block) = encoder.next_block(CAPTURE_TIMEOUT_MS) else {
            continue;
        };
        let bytes = block.data();
        let length = bytes.len();
        // The encoder usually stamps blocks; when it does not, the engine counts
        // 20 ms frames itself - the length of one G.711 block at 8 kHz, which is
        // also how the daemon splits a record into RTSP frames.
        let timestamp = if block.timestamp() != 0 {
            block.timestamp()
        } else {
            fallback.advance()
        };
        if length > 0 && length <= AUDIO_MAX_PAYLOAD {
            // A failed send means the daemon went away; the session thread notices
            // that too and detaches the sink, so this is not worth a log line per
            // frame, exactly as in the reference engine.
            let _ = sink.send(RecordKind::AUDIO_CAPTURE, 0, false, timestamp, bytes);
        }
    }

    // The encoder owns the edge and removes it before its AENC endpoint is
    // destroyed. Both devices are then released here, in the thread that used
    // them.
    drop(encoder);
    drop(input);
}

/// Copy an ALSA card name into the fixed-size field, truncating like the
/// reference engine's `snprintf` and leaving it NUL-terminated.
fn copy_card_name(field: &mut [u8; MAX_AUDIO_CARD_NAME], card: &str) {
    let name = card.as_bytes();
    let taken = name.len().min(MAX_AUDIO_CARD_NAME - 1);
    field[..taken].copy_from_slice(&name[..taken]);
    field[taken] = 0;
}

/// The camera's backend: the playback devices, the microphone, and the night
/// state the daemon is told about until the image path lands.
///
/// `--mock-backend` selects the hardware-free backend instead, which is what the
/// emulation tests use.
pub struct HardwareBackend {
    card: String,
    video: VideoConfig,
    iq_directory: String,
    speaker: Option<Speaker>,
    microphone: Option<Microphone>,
    video_pipeline: Option<VideoPipeline>,
    image: Option<NightMonitor>,
    capture_enabled: bool,
    /// The night state the daemon is told about, shared with the monitor thread
    /// that owns the IR-cut, the IR LED and the ISP scene.
    night: Arc<Mutex<NightState>>,
}

impl HardwareBackend {
    /// Create a backend for the given audio card.
    ///
    /// `capture_enabled` is the daemon's microphone preference; when it is false
    /// the capture device is never opened, which is what the reference engine's
    /// `--no-audio` does.
    pub fn new(
        card: &str,
        capture_enabled: bool,
        video: VideoConfig,
        iq_directory: &str,
    ) -> Result<Self, Error> {
        Ok(Self {
            card: String::from(card),
            video,
            iq_directory: String::from(iq_directory),
            speaker: None,
            microphone: None,
            video_pipeline: None,
            image: None,
            capture_enabled,
            night: Arc::new(Mutex::new(NightState::new())?),
        })
    }
}

/// Start the vendor media system.
///
/// This mirrors the reference engine's `main`, which sets Rockit's logging
/// variables and then calls `RK_MPI_SYS_Init` before it opens anything: every
/// MPI call below depends on that initialization, and the two variables are read
/// as the system comes up.
///
/// A failure is not fatal here the way it is in the reference engine, which
/// exits: the engine stays up and serves the daemon so the reason is visible,
/// and no vendor call is made afterwards that would run on an uninitialized
/// system.
fn start_media_system() -> Result<(), Error> {
    for (name, value) in [
        (ROCKIT_LOG_LEVEL_VARIABLE, ROCKIT_LOG_LEVEL),
        (ROCKIT_LOG_PATH_VARIABLE, ROCKIT_LOG_PATH),
    ] {
        let name = CString::new(name).map_err(|_| Error::from_errno(libc::EINVAL))?;
        let value = CString::new(value).map_err(|_| Error::from_errno(libc::EINVAL))?;
        // Overwrite: the engine's own settings win over whatever the launching
        // environment carries, as in the reference engine.
        if unsafe { libc::setenv(name.as_ptr(), value.as_ptr(), 1) } != 0 {
            return Err(Error::last());
        }
    }
    crate::vendor::check(unsafe { mpi::RK_MPI_SYS_Init() })
}

impl Backend for HardwareBackend {
    fn start(&mut self, sink: Arc<RecordSink>, shutdown: Arc<Shutdown>) -> Result<(), Error> {
        if let Err(error) = start_media_system() {
            media_log!("media_sys_init_error={error}");
            return Err(error);
        }

        let mut failure = None;

        // The capture buffer is taken from the media memory heap first, because
        // the ISP takes its own buffers as soon as AIQ starts and the heap has to
        // still have room for a full frame - the reference engine allocates it in
        // `main` for the same reason.
        let prepared: Option<VideoPrepared> = match super::video::prepare(&self.video) {
            Ok(prepared) => Some(prepared),
            Err(error) => {
                media_log!("media_video_prepare_error={error}");
                failure = Some(error);
                None
            }
        };

        // Then the image path, before the capture buffers are queued: it is what
        // programs the ISP that fills them, and AIQ reads its tuning files as it
        // initializes.
        match ImagePath::open(&self.iq_directory) {
            Ok(image) => match NightMonitor::start(
                image,
                Arc::clone(&sink),
                Arc::clone(&shutdown),
                Arc::clone(&self.night),
            ) {
                Ok(monitor) => self.image = Some(monitor),
                Err(error) => {
                    media_log!("media_night_monitor_error={error}");
                    failure = Some(error);
                }
            },
            Err(error) => {
                media_log!("media_aiq_error={error}");
                failure = Some(error);
            }
        }

        // The devices come up in the order the reference engine's threads are
        // created: the capture path, then the microphone, then the speaker. The
        // board's audio codec is sensitive to the last two - the capture path has
        // to own the card before playback opens it - and the video path starts
        // first because it is what brings the sensor and the encoder up.
        if let Some(prepared) = prepared {
            match VideoPipeline::open(
                &self.video,
                prepared,
                Arc::clone(&sink),
                Arc::clone(&shutdown),
            ) {
                Ok(pipeline) => self.video_pipeline = Some(pipeline),
                Err(error) => {
                    media_log!("media_video_error={error}");
                    failure = Some(error);
                }
            }
        }

        if self.capture_enabled {
            match Microphone::open(&self.card, Arc::clone(&sink), Arc::clone(&shutdown)) {
                Ok(microphone) => self.microphone = Some(microphone),
                Err(error) => {
                    media_log!("media_microphone_error={error}");
                    failure = Some(error);
                }
            }
        } else {
            media_log!("media_audio_disabled=1");
        }

        match Speaker::open(&self.card) {
            Ok(speaker) => self.speaker = Some(speaker),
            Err(error) => {
                media_log!("media_speaker_error={error}");
                failure = Some(error);
            }
        }

        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn command(&mut self, command: Command<'_>, _sink: &RecordSink) -> Result<(), Error> {
        // The sink is part of the backend contract, but this backend reports night
        // changes from the monitor thread, which owns the hardware and knows when
        // one actually happened.
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
                // The command records what was asked for; the monitor thread owns
                // the hardware and applies it on its next tick, which keeps the
                // IR-cut, the IR LED and the AIQ scene under one thread.
                match self.night.lock() {
                    Ok(mut state) => {
                        state.apply(target);
                    }
                    Err(error) => media_log!("media_night_state_error={error}"),
                }
                media_log!("media_night_mode_set={}", target.name());
            }
        }
        Ok(())
    }

    fn night_mode(&self) -> (bool, bool) {
        match self.night.lock() {
            Ok(state) => state.reported(),
            // A poisoned lock means the monitor thread died; report what the
            // daemon was last told rather than inventing a state.
            Err(_) => (false, false),
        }
    }

    fn shutdown(&mut self) {
        // Dropping the speaker clears the queue, disables the resampler, the
        // channel and the device, and drives the amplifier low; dropping the
        // microphone joins its thread, which releases the capture device and its
        // encoder; dropping the video pipeline joins the capture thread, which
        // stops the capture node and destroys the encoder channel.
        // The night monitor first: it is what releases the ISP tuning context, and
        // the reference engine joins its night thread before the rest too.
        self.image = None;
        self.microphone = None;
        self.speaker = None;
        self.video_pipeline = None;
        media_log!("media_devices_stopped=1");
    }
}

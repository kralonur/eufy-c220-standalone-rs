//! Bring-up probe for the media engine's private socket.
//!
//! The firmware is the engine's only real client, and on a camera that is
//! running the deployed firmware the engine cannot be reached at all: one
//! session at a time means a second connection waits. This probe is what talks to
//! a hand-started engine during hardware bring-up, before the firmware is in the
//! loop, and it is small on purpose:
//!
//! * `greeting` prints the record the engine sends on connect, which is the
//!   protocol handshake and the night-mode state in one line.
//! * `drain [seconds]` counts what the engine produces per kind - the only way to
//!   see microphone records before the RTSP path can carry them, because the
//!   firmware drops the audio track it cannot advertise without video.
//! * `tone [seconds]` plays a 1 kHz tone through the speaker, which exercises the
//!   whole talkback path (record, FDK-less PCM, AO resampler, amplifier pin)
//!   without a daemon.
//! * `aac <file>` sends an ADTS file's access units, which is the only way to
//!   exercise the vendor AAC decoder without a daemon.
//! * `volume <0-100>` and `clear` drive the other two speaker commands.
//!
//! It is a development tool: it links `std`, so it builds as a static musl binary
//! for the camera, where the TLS-free requirement applies only to the engine's
//! own dynamically linked executable.

use c220_media_protocol::{FRAME_HEADER_LEN, Header, RecordKind};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Where the engine listens unless `--socket` says otherwise.
const DEFAULT_SOCKET: &str = "/tmp/c220-media.sock";

/// Samples per speaker record: 20 ms at 16 kHz, the daemon's own cadence.
const TONE_SAMPLES: usize = 320;

/// Rate the talkback path is fed at, in samples per second.
const TONE_RATE: f64 = 16_000.0;

/// Frequency of the test tone, in hertz.
const TONE_HZ: f64 = 1_000.0;

/// Length of an ADTS header without a CRC, in bytes. The frame length it carries
/// is 13 bits wide, split across the third, fourth and fifth bytes, and counts
/// the header itself.
const ADTS_HEADER_LEN: usize = 7;

/// Amplitude of the test tone, as a fraction of full scale.
const TONE_AMPLITUDE: f64 = 0.25;

/// A record the probe read.
struct Record {
    header: Header,
    payload: Vec<u8>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("media_probe: {error}");
        std::process::exit(1);
    }
}

fn run() -> std::io::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let mut socket_path = String::from(DEFAULT_SOCKET);
    let mut command = None;
    let mut options: Vec<String> = Vec::new();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--socket" => {
                socket_path = arguments.next().ok_or_else(|| {
                    std::io::Error::new(ErrorKind::InvalidInput, "--socket requires a path")
                })?;
            }
            _ if command.is_none() => command = Some(argument),
            _ => options.push(argument),
        }
    }

    match command.as_deref().unwrap_or("greeting") {
        "greeting" => {
            let (stream, record) = connect_and_greet(&socket_path)?;
            let _ = stream;
            print_record(&record);
        }
        "drain" => {
            let seconds = seconds_of(&options, 5)?;
            let (stream, record) = connect_and_greet(&socket_path)?;
            print_record(&record);
            drain(&stream, seconds)?;
        }
        "tone" => {
            let seconds = seconds_of(&options, 2)?;
            let (stream, _) = connect_and_greet(&socket_path)?;
            tone(&stream, seconds)?;
        }
        "aac" => {
            let path = options.first().ok_or_else(|| {
                std::io::Error::new(ErrorKind::InvalidInput, "aac needs an ADTS file")
            })?;
            let adts = std::fs::read(path)?;
            let (stream, _) = connect_and_greet(&socket_path)?;
            aac(&stream, &adts)?;
        }
        "volume" => {
            let volume: u32 = options
                .first()
                .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "volume needs 0-100"))?
                .parse()
                .map_err(|_| {
                    std::io::Error::new(ErrorKind::InvalidInput, "volume is not a number")
                })?;
            let (stream, _) = connect_and_greet(&socket_path)?;
            write_record(
                &stream,
                RecordKind::SPEAKER_VOLUME,
                false,
                0,
                &volume.to_le_bytes(),
            )?;
            println!("sent volume={volume}");
        }
        "clear" => {
            let (stream, _) = connect_and_greet(&socket_path)?;
            write_record(&stream, RecordKind::SPEAKER_CLEAR, false, 0, &[])?;
            println!("sent clear");
        }
        other => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!("unknown command {other}: use greeting, drain, tone, aac, volume or clear"),
            ));
        }
    }
    Ok(())
}

fn seconds_of(options: &[String], default: u64) -> std::io::Result<u64> {
    match options.first() {
        Some(value) => value
            .parse()
            .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "not a number of seconds")),
        None => Ok(default),
    }
}

/// Connect, read the greeting record, and keep the stream open.
///
/// Every mode reads the greeting, including the ones that only send: the engine
/// writes it on connect whether or not the client wants it, and a client that has
/// already gone away turns that into a `media_greeting_error` line.
fn connect_and_greet(path: &str) -> std::io::Result<(UnixStream, Record)> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let record = read_record(&stream)?;
    Ok((stream, record))
}

/// Read one record, waiting for it.
fn read_record(stream: &UnixStream) -> std::io::Result<Record> {
    let mut bytes = [0_u8; FRAME_HEADER_LEN];
    read_exact(stream, &mut bytes, None)?;
    let header = Header::parse(&bytes).map_err(|error| {
        std::io::Error::new(ErrorKind::InvalidData, format!("bad header: {error}"))
    })?;
    let mut payload = vec![0_u8; header.payload_len()];
    if !payload.is_empty() {
        read_exact(stream, &mut payload, None)?;
    }
    Ok(Record { header, payload })
}

/// Fill `buffer`, treating the read timeout as "nothing yet" while `deadline`
/// has not passed.
fn read_exact(
    stream: &UnixStream,
    buffer: &mut [u8],
    deadline: Option<Instant>,
) -> std::io::Result<()> {
    let mut filled = 0;
    // `Read` is implemented for `&UnixStream`, so a shared borrow is enough here.
    let mut reader = stream;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "engine closed the socket",
                ));
            }
            Ok(count) => filled += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Err(std::io::Error::new(ErrorKind::TimedOut, "nothing to read"));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Print what the engine produced until the deadline.
fn drain(stream: &UnixStream, seconds: u64) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut counts: BTreeMap<u8, u64> = BTreeMap::new();
    let mut audio_bytes: (usize, usize) = (usize::MAX, 0);
    let mut video_keyframes = 0_u64;
    let started = Instant::now();

    while Instant::now() < deadline {
        let mut bytes = [0_u8; FRAME_HEADER_LEN];
        match read_exact(stream, &mut bytes, Some(deadline)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::TimedOut => break,
            Err(error) => return Err(error),
        }
        let header = Header::parse(&bytes).map_err(|error| {
            std::io::Error::new(ErrorKind::InvalidData, format!("bad header: {error}"))
        })?;
        let mut payload = vec![0_u8; header.payload_len()];
        if !payload.is_empty() {
            read_exact(stream, &mut payload, Some(deadline))?;
        }
        *counts.entry(header.kind().as_u8()).or_default() += 1;
        if header.kind() == RecordKind::AUDIO_CAPTURE && !payload.is_empty() {
            audio_bytes = (
                audio_bytes.0.min(payload.len()),
                audio_bytes.1.max(payload.len()),
            );
        }
        if header.kind() == RecordKind::VIDEO && header.keyframe() {
            video_keyframes += 1;
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    println!("drained for {elapsed:.1}s");
    for (kind, count) in &counts {
        println!(
            "  kind={kind} {} count={count} rate={:.1}/s",
            kind_name(*kind),
            *count as f64 / elapsed
        );
    }
    if audio_bytes.0 != usize::MAX {
        println!(
            "  audio payload bytes: min={} max={} (one G.711 block is 160 bytes = 20 ms)",
            audio_bytes.0, audio_bytes.1
        );
    }
    println!("  video keyframes={video_keyframes}");
    Ok(())
}

/// Play an ADTS file's access units through the talkback decoder.
///
/// This is the mode that exercises the vendor AAC decoder, which the daemon
/// otherwise reaches through its `/talkback/audio` endpoint: one access unit per
/// record, with the keyframe flag clear, paced at the 64 ms cadence of a
/// 1024-sample frame at 16 kHz.
fn aac(stream: &UnixStream, adts: &[u8]) -> std::io::Result<()> {
    let mut offset = 0;
    let mut timestamp_us = 0_u64;
    let mut frames = 0_u64;
    while offset + ADTS_HEADER_LEN <= adts.len() {
        if adts[offset] != 0xff || adts[offset + 1] & 0xf0 != 0xf0 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("no ADTS sync word at offset {offset}"),
            ));
        }
        let low = usize::from(adts[offset + 3]) & 0x03;
        let middle = usize::from(adts[offset + 4]);
        let high = usize::from(adts[offset + 5]) >> 5;
        // The frame length includes the header, so it is also the step to the
        // next sync word.
        let length = (low << 11) | (middle << 3) | high;
        if offset + length > adts.len() {
            break;
        }
        write_record(
            stream,
            RecordKind::SPEAKER_PCM,
            false,
            timestamp_us,
            &adts[offset..offset + length],
        )?;
        offset += length;
        timestamp_us += 64_000;
        frames += 1;
        std::thread::sleep(Duration::from_millis(64));
    }
    println!("sent {frames} AAC access units");
    Ok(())
}

/// Play a tone through the camera's speaker.
fn tone(stream: &UnixStream, seconds: u64) -> std::io::Result<()> {
    let records = seconds * 50;
    let mut timestamp_us = 0_u64;
    for index in 0..records {
        let mut payload = Vec::with_capacity(TONE_SAMPLES * 2);
        for sample in 0..TONE_SAMPLES {
            let position = (index as usize * TONE_SAMPLES + sample) as f64 / TONE_RATE;
            let value = (position * TONE_HZ * std::f64::consts::TAU).sin() * TONE_AMPLITUDE;
            payload.extend_from_slice(&((value * f64::from(i16::MAX)) as i16).to_le_bytes());
        }
        // Raw PCM is marked by the keyframe flag on this record kind; the daemon
        // sets it the same way.
        write_record(
            stream,
            RecordKind::SPEAKER_PCM,
            true,
            timestamp_us,
            &payload,
        )?;
        timestamp_us += 20_000;
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("sent {records} PCM records ({seconds}s of {TONE_HZ:.0} Hz)");
    Ok(())
}

fn write_record(
    stream: &UnixStream,
    kind: RecordKind,
    keyframe: bool,
    timestamp_us: u64,
    payload: &[u8],
) -> std::io::Result<()> {
    let header = Header::new(payload.len(), timestamp_us, keyframe, kind, 0)
        .map_err(|error| std::io::Error::new(ErrorKind::InvalidInput, format!("{error}")))?;
    let mut writer = stream;
    writer.write_all(&header.encode())?;
    writer.write_all(payload)
}

fn print_record(record: &Record) {
    let kind = record.header.kind();
    println!(
        "record kind={} ({}) keyframe={} codec={} timestamp_us={} payload={}",
        kind.as_u8(),
        kind_name(kind.as_u8()),
        record.header.keyframe(),
        record.header.codec(),
        record.header.timestamp_us(),
        record.payload.len()
    );
    if !record.payload.is_empty() && record.payload.len() <= 16 {
        println!("  payload={:02x?}", record.payload);
    }
}

/// The ABI names, for a readable summary.
fn kind_name(kind: u8) -> &'static str {
    match kind {
        0 => "video",
        1 => "audio-capture",
        2 => "speaker-pcm",
        3 => "speaker-clear",
        4 => "speaker-volume",
        6 => "night-mode",
        _ => "unknown",
    }
}

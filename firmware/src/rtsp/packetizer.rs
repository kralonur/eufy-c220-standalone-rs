//! Stateful RTP framing into one reusable access-unit buffer.
//! No intermediate NAL index, rebuilt bitstream or heap-allocated fragments.

use super::{
    AUDIO_FRAME_LEN, AudioFrame, RTP_AUDIO_CLOCK_RATE, RTP_AUDIO_PAYLOAD_TYPE,
    RTP_AUDIO_TIMESTAMP_INCREMENT, RTP_HEADER_LEN, RTP_PAYLOAD_MTU, RTP_VIDEO_CLOCK_RATE,
    RTP_VIDEO_PAYLOAD_TYPE, RTP_VIDEO_TIMESTAMP_INCREMENT, VideoCodec, VideoFrame, annex_b_nalus,
};
use bytes::Bytes;
use std::io;

/// RFC 2326 interleaved prefix: magic, channel, and a big-endian u16 length.
pub(super) const INTERLEAVED_HEADER_LEN: usize = 4;
/// RFC 7798 fragmentation unit: two-byte payload header and one-byte FU header.
const H265_FU_HEADER_LEN: usize = 3;

// RFC 6184/7798 aggregation lengths are 16-bit. Bound the out-of-band cache
// before copying, rather than allowing three protocol-sized (4-MiB) NALs.
const MAX_PARAMETER_SET_BYTES: usize = u16::MAX as usize;
// RFC 7798's FU indicator/header is three bytes (H.264 needs only two).
const MAX_FU_HEADER_BYTES: usize = H265_FU_HEADER_LEN;
// A maximum access unit plus fragmentation/framing overhead and one parameter
// aggregate. This also bounds amplification by many tiny VCL NALs.
const MAX_BATCH_BYTES: usize = c220_media_protocol::MAX_PAYLOAD_LEN
    + (c220_media_protocol::MAX_PAYLOAD_LEN / (RTP_PAYLOAD_MTU - MAX_FU_HEADER_BYTES) + 1)
        * (INTERLEAVED_HEADER_LEN + RTP_HEADER_LEN + MAX_FU_HEADER_BYTES)
    + INTERLEAVED_HEADER_LEN
    + RTP_HEADER_LEN
    + RTP_PAYLOAD_MTU;

// RFC 3550: version two in the high bits of the first RTP header byte.
const RTP_VERSION_BITS: u8 = 0x80;
// RFC 3550: marker occupies the high bit of the payload-type byte.
const RTP_MARKER_BIT: u8 = 0x80;
// RFC 6184/7798: the FU header flags the first fragment.
const FU_START_BIT: u8 = 0x80;
// RFC 6184/7798: the FU header end flag is bit six.
const FU_END_BIT: u8 = 0x40;
// RFC 6184: H.264 NAL type occupies the low five bits.
const H264_TYPE_MASK: u8 = 0x1f;
// RFC 6184: forbidden_zero_bit must be clear in a valid NAL header.
const H264_FORBIDDEN_BIT: u8 = 0x80;
// RFC 6184: NRI occupies bits five and six of a NAL header.
const H264_NRI_MASK: u8 = 0x60;
// RFC 6184: preserve F/NRI when replacing a NAL type with FU-A.
const H264_FU_INDICATOR_MASK: u8 = 0xe0;
// RFC 6184: coded IDR slices have type five; VCL types span one to five.
const H264_IDR_TYPE: u8 = 5;
// RFC 6184: sequence parameter set NAL type.
const H264_SPS_TYPE: u8 = 7;
// RFC 6184: picture parameter set NAL type.
const H264_PPS_TYPE: u8 = 8;
// RFC 6184: types one through 23 may be carried as single NAL packets.
const H264_LAST_SINGLE_NAL_TYPE: u8 = 23;
// RFC 6184: single-time aggregation packet A type.
const H264_STAP_A_TYPE: u8 = 24;
// RFC 6184: fragmentation unit A type.
const H264_FU_A_TYPE: u8 = 28;
// RFC 7798: HEVC NAL type occupies six bits after forbidden_zero_bit.
const H265_TYPE_MASK: u8 = 0x3f;
// RFC 7798: preserve F and the high layer-id bit when creating a FU.
const H265_FU_INDICATOR_MASK: u8 = 0x81;
// H.265 table 7-1: types zero through 31 are VCL NAL units.
const H265_LAST_VCL_TYPE: u8 = 31;
// H.265 table 7-1: the random-access range spans BLA through reserved IRAP.
const H265_IRAP_TYPES: core::ops::RangeInclusive<u8> = 16..=23;
// RFC 7798: video parameter set NAL type.
const H265_VPS_TYPE: u8 = 32;
// RFC 7798: sequence parameter set NAL type.
const H265_SPS_TYPE: u8 = 33;
// RFC 7798: picture parameter set NAL type.
const H265_PPS_TYPE: u8 = 34;
// RFC 7798: aggregation packet NAL type.
const H265_AP_TYPE: u8 = 48;
// RFC 7798: fragmentation unit NAL type.
const H265_FU_TYPE: u8 = 49;

#[derive(Clone, Debug, Default)]
pub(super) struct CodecState {
    pub codec: VideoCodec,
    pub vps: Option<Bytes>,
    pub sps: Option<Bytes>,
    pub pps: Option<Bytes>,
}

pub(super) struct Inspection {
    pub changed: bool,
    pub keyframe: bool,
}

impl CodecState {
    pub fn has_parameters(&self) -> bool {
        self.sps.is_some()
            && self.pps.is_some()
            && (self.codec == VideoCodec::H264 || self.vps.is_some())
    }

    pub fn inspect(&mut self, codec: VideoCodec, data: &[u8]) -> io::Result<Inspection> {
        let mut info = Inspection {
            changed: false,
            keyframe: false,
        };
        if self.codec != codec {
            *self = Self {
                codec,
                ..Self::default()
            };
            info.changed = true;
        }
        for nal in annex_b_nalus(data) {
            let Some(kind) = nal_type(codec, nal) else {
                continue;
            };
            info.changed |= self.update_parameter(kind, nal)?;
            info.keyframe |= match codec {
                VideoCodec::H264 => kind == H264_IDR_TYPE,
                VideoCodec::H265 => H265_IRAP_TYPES.contains(&kind),
            };
        }
        Ok(info)
    }

    fn update_parameter(&mut self, kind: u8, nal: &[u8]) -> io::Result<bool> {
        let parameter = match (self.codec, kind) {
            (VideoCodec::H264, H264_SPS_TYPE) | (VideoCodec::H265, H265_SPS_TYPE) => &mut self.sps,
            (VideoCodec::H264, H264_PPS_TYPE) | (VideoCodec::H265, H265_PPS_TYPE) => &mut self.pps,
            (VideoCodec::H265, H265_VPS_TYPE) => &mut self.vps,
            _ => return Ok(false),
        };
        if nal.len() > MAX_PARAMETER_SET_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "video parameter set exceeds RTP length limit",
            ));
        }
        if parameter.as_deref() == Some(nal) {
            return Ok(false);
        }
        *parameter = Some(Bytes::copy_from_slice(nal));
        Ok(true)
    }
}

fn nal_type(codec: VideoCodec, nal: &[u8]) -> Option<u8> {
    match codec {
        VideoCodec::H264 => nal
            .first()
            .filter(|byte| **byte & H264_FORBIDDEN_BIT == 0)
            .map(|byte| byte & H264_TYPE_MASK)
            .filter(|kind| (1..=H264_LAST_SINGLE_NAL_TYPE).contains(kind)),
        VideoCodec::H265 => (nal.len() >= 2).then(|| (nal[0] >> 1) & H265_TYPE_MASK),
    }
}

fn is_vcl(codec: VideoCodec, kind: u8) -> bool {
    match codec {
        VideoCodec::H264 => (1..=H264_IDR_TYPE).contains(&kind),
        VideoCodec::H265 => kind <= H265_LAST_VCL_TYPE,
    }
}

/// Extended timestamps are compared before truncation to RTP's 32-bit wire
/// value. Numeric comparison of wire values freezes progress at the 13-hour
/// video-clock wrap boundary.
#[derive(Default)]
struct RtpClock {
    value: u64,
    base_rtp: u64,
    source_base_us: Option<u64>,
    last_source_us: Option<u64>,
}

impl RtpClock {
    fn timestamp(&mut self, source_us: Option<u64>, rate: u32, fallback: u32) -> u32 {
        let Some(source_us) = source_us else {
            let timestamp = self.value.saturating_add(if self.source_base_us.is_some() {
                u64::from(fallback)
            } else {
                0
            });
            self.value = timestamp.saturating_add(u64::from(fallback));
            // Resuming source timestamps starts from the fallback clock, not a
            // stale capture epoch. This preserves the C220's existing policy.
            self.source_base_us = None;
            self.last_source_us = None;
            return timestamp as u32;
        };
        let previous = self.last_source_us;
        let source_us = previous.map_or(source_us, |last| source_us.max(last));
        self.last_source_us = Some(source_us);
        let base = *self.source_base_us.get_or_insert_with(|| {
            self.base_rtp = self.value;
            source_us
        });
        let elapsed = source_us - base;
        let ticks = (elapsed / 1_000_000)
            .saturating_mul(u64::from(rate))
            .saturating_add((elapsed % 1_000_000) * u64::from(rate) / 1_000_000);
        let timestamp = self.base_rtp.saturating_add(ticks);
        self.value = if previous.is_some() && timestamp <= self.value {
            self.value.saturating_add(1)
        } else {
            timestamp
        };
        self.value as u32
    }
}

struct RtpStream {
    sequence: u16,
    ssrc: u32,
    clock: RtpClock,
}

impl RtpStream {
    fn new(ssrc: u32) -> Self {
        Self {
            sequence: 0,
            ssrc,
            clock: RtpClock::default(),
        }
    }

    fn append(
        &mut self,
        batch: &mut Vec<u8>,
        channel: u8,
        payload_type: u8,
        timestamp: u32,
        parts: &[&[u8]],
    ) -> io::Result<usize> {
        let payload_len: usize = parts.iter().map(|part| part.len()).sum();
        let length = u16::try_from(RTP_HEADER_LEN + payload_len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "RTP packet too large"))?;
        let total = INTERLEAVED_HEADER_LEN + usize::from(length);
        if batch.len().saturating_add(total) > MAX_BATCH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RTP access unit exceeds batch byte limit",
            ));
        }
        let start = batch.len();
        if batch.capacity() < start + total {
            // Geometric reuse without Vec's final doubling retaining capacity
            // beyond the batch bound after one unusually large keyframe.
            let capacity = batch
                .capacity()
                .saturating_mul(2)
                .clamp(start + total, MAX_BATCH_BYTES);
            batch.reserve_exact(capacity - start);
        }
        batch.extend_from_slice(&[b'$', channel]);
        batch.extend_from_slice(&length.to_be_bytes());
        batch.extend_from_slice(&[RTP_VERSION_BITS, payload_type]);
        batch.extend_from_slice(&self.sequence.to_be_bytes());
        batch.extend_from_slice(&timestamp.to_be_bytes());
        batch.extend_from_slice(&self.ssrc.to_be_bytes());
        for part in parts {
            batch.extend_from_slice(part);
        }
        self.sequence = self.sequence.wrapping_add(1);
        Ok(start)
    }
}

pub(super) struct VideoPacketizer {
    parameters: CodecState,
    stream: RtpStream,
}

impl VideoPacketizer {
    pub fn new(codec: VideoCodec, ssrc: u32) -> Self {
        Self {
            parameters: CodecState {
                codec,
                ..CodecState::default()
            },
            stream: RtpStream::new(ssrc),
        }
    }

    pub fn codec(&self) -> VideoCodec {
        self.parameters.codec
    }

    pub fn append_rtp_into(
        &mut self,
        frame: &VideoFrame,
        channel: u8,
        batch: &mut Vec<u8>,
    ) -> io::Result<()> {
        let timestamp = self.stream.clock.timestamp(
            frame.timestamp_us,
            RTP_VIDEO_CLOCK_RATE,
            RTP_VIDEO_TIMESTAMP_INCREMENT,
        );
        let mut last_packet = None;
        for nal in annex_b_nalus(&frame.data) {
            let Some(kind) = nal_type(frame.codec, nal) else {
                continue;
            };
            self.parameters.update_parameter(kind, nal)?;
            if !is_vcl(frame.codec, kind) {
                continue;
            }
            // Parameter sets precede the slices that use them in Annex-B.
            // Update and packetize in one pass; a second whole-frame scan made
            // the HEVC path slower despite eliminating fragment allocations.
            let random_access = match frame.codec {
                VideoCodec::H264 => kind == H264_IDR_TYPE,
                VideoCodec::H265 => H265_IRAP_TYPES.contains(&kind),
            };
            if last_packet.is_none()
                && (frame.keyframe || random_access)
                && self.parameters.has_parameters()
            {
                self.append_parameters(channel, timestamp, batch)?;
            }
            if nal.len() <= RTP_PAYLOAD_MTU {
                last_packet = Some(self.stream.append(
                    batch,
                    channel,
                    RTP_VIDEO_PAYLOAD_TYPE,
                    timestamp,
                    &[nal],
                )?);
                continue;
            }
            // RFC 6184 FU-A and RFC 7798 FU: borrow each source fragment and
            // append its small stack header directly into the final batch.
            let (skip, header_len) = match frame.codec {
                VideoCodec::H264 => (1, 2),
                VideoCodec::H265 => (2, 3),
            };
            let chunks = nal[skip..].chunks(RTP_PAYLOAD_MTU - header_len);
            let count = chunks.len();
            for (index, payload) in chunks.enumerate() {
                let flags = if index == 0 { FU_START_BIT } else { 0 }
                    | if index + 1 == count { FU_END_BIT } else { 0 };
                let header = match frame.codec {
                    VideoCodec::H264 => [
                        (nal[0] & H264_FU_INDICATOR_MASK) | H264_FU_A_TYPE,
                        kind | flags,
                        0,
                    ],
                    VideoCodec::H265 => [
                        (nal[0] & H265_FU_INDICATOR_MASK) | (H265_FU_TYPE << 1),
                        nal[1],
                        kind | flags,
                    ],
                };
                last_packet = Some(self.stream.append(
                    batch,
                    channel,
                    RTP_VIDEO_PAYLOAD_TYPE,
                    timestamp,
                    &[&header[..header_len], payload],
                )?);
            }
        }
        if let Some(start) = last_packet {
            // RFC 3550 marker belongs to the final VCL packet, not the parameter
            // aggregate or every slice. Byte one follows the interleaved header.
            batch[start + INTERLEAVED_HEADER_LEN + 1] |= RTP_MARKER_BIT;
        }
        Ok(())
    }

    fn append_parameters(
        &mut self,
        channel: u8,
        timestamp: u32,
        batch: &mut Vec<u8>,
    ) -> io::Result<()> {
        let sets = match self.codec() {
            VideoCodec::H264 => [
                None,
                self.parameters.sps.as_deref(),
                self.parameters.pps.as_deref(),
            ],
            VideoCodec::H265 => [
                self.parameters.vps.as_deref(),
                self.parameters.sps.as_deref(),
                self.parameters.pps.as_deref(),
            ],
        };
        let header_len = if self.codec() == VideoCodec::H264 {
            1
        } else {
            2
        };
        let length = header_len
            + sets
                .iter()
                .flatten()
                .map(|nal| 2 + nal.len())
                .sum::<usize>();
        if length > RTP_PAYLOAD_MTU {
            return Ok(());
        }
        // Parameter aggregate is small and bounded to one MTU. Build it on the
        // stack so repeated IDRs allocate neither cache replacements nor packets.
        let mut payload = [0_u8; RTP_PAYLOAD_MTU];
        payload[0] = match self.codec() {
            // RFC 6184 requires the greatest NRI of the aggregated NALs.
            VideoCodec::H264 => {
                sets.iter()
                    .flatten()
                    .map(|nal| nal[0] & H264_NRI_MASK)
                    .max()
                    .unwrap_or_default()
                    | H264_STAP_A_TYPE
            }
            VideoCodec::H265 => H265_AP_TYPE << 1,
        };
        payload[1] = 1;
        let mut offset = header_len;
        for nal in sets.into_iter().flatten() {
            payload[offset..offset + 2].copy_from_slice(&(nal.len() as u16).to_be_bytes());
            offset += 2;
            payload[offset..offset + nal.len()].copy_from_slice(nal);
            offset += nal.len();
        }
        self.stream.append(
            batch,
            channel,
            RTP_VIDEO_PAYLOAD_TYPE,
            timestamp,
            &[&payload[..offset]],
        )?;
        Ok(())
    }
}

pub(super) struct AudioPacketizer {
    stream: RtpStream,
}

impl AudioPacketizer {
    pub fn new(ssrc: u32) -> Self {
        Self {
            stream: RtpStream::new(ssrc),
        }
    }

    pub fn append_rtp_into(
        &mut self,
        frame: &AudioFrame,
        channel: u8,
        batch: &mut Vec<u8>,
    ) -> io::Result<()> {
        debug_assert_eq!(frame.data.len(), AUDIO_FRAME_LEN);
        let timestamp = self.stream.clock.timestamp(
            frame.timestamp_us,
            RTP_AUDIO_CLOCK_RATE,
            RTP_AUDIO_TIMESTAMP_INCREMENT,
        );
        self.stream.append(
            batch,
            channel,
            RTP_AUDIO_PAYLOAD_TYPE,
            timestamp,
            &[&frame.data],
        )?;
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn rtp_packets(mut batch: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    while !batch.is_empty() {
        let (frame, used) = rtsp_runtime::InterleavedFrame::parse(batch)
            .expect("valid interleaved fixture")
            .expect("complete interleaved fixture");
        packets.push(frame.payload);
        batch = &batch[used..];
    }
    packets
}

#[cfg(test)]
pub(super) fn rtp_packet(
    payload_type: u8,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut stream = RtpStream::new(ssrc);
    stream.sequence = sequence;
    let mut batch = Vec::new();
    stream
        .append(&mut batch, 0, payload_type, timestamp, &[payload])
        .expect("bounded RTP fixture");
    let mut packet = batch[INTERLEAVED_HEADER_LEN..].to_vec();
    if marker {
        packet[1] |= RTP_MARKER_BIT;
    }
    packet
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod bounds_tests {
    use super::*;

    // A normal RTP video clock wraps after ~13.25 hours. Comparing wire values
    // numerically used to advance it by one tick per frame instead of 4,500.
    #[test]
    fn video_timestamps_keep_their_cadence_across_wire_wrap() {
        let mut clock = RtpClock::default();
        assert_eq!(
            clock.timestamp(Some(0), RTP_VIDEO_CLOCK_RATE, RTP_VIDEO_TIMESTAMP_INCREMENT),
            0
        );
        // Derive the boundary from the 32-bit RTP clock, in microseconds; put
        // consecutive 20-fps source frames starting one millisecond before wrap.
        let wrap_us = u64::from(u32::MAX) * 1_000_000 / u64::from(RTP_VIDEO_CLOCK_RATE);
        let before_us = wrap_us - 1_000;
        let frame_us =
            u64::from(RTP_VIDEO_TIMESTAMP_INCREMENT) * 1_000_000 / u64::from(RTP_VIDEO_CLOCK_RATE);
        let before = clock.timestamp(
            Some(before_us),
            RTP_VIDEO_CLOCK_RATE,
            RTP_VIDEO_TIMESTAMP_INCREMENT,
        );
        let after = clock.timestamp(
            Some(before_us + frame_us),
            RTP_VIDEO_CLOCK_RATE,
            RTP_VIDEO_TIMESTAMP_INCREMENT,
        );
        assert!(after < before);
        assert_eq!(after.wrapping_sub(before), RTP_VIDEO_TIMESTAMP_INCREMENT);
    }

    #[test]
    fn timestamp_fallback_tracks_actual_audio_samples_and_source_resumption() {
        let mut clock = RtpClock::default();
        assert_eq!(clock.timestamp(None, RTP_AUDIO_CLOCK_RATE, 480), 0);
        assert_eq!(clock.timestamp(None, RTP_AUDIO_CLOCK_RATE, 80), 480);
        assert_eq!(
            clock.timestamp(Some(1_000_000), RTP_AUDIO_CLOCK_RATE, 160),
            560
        );
        assert_eq!(
            clock.timestamp(Some(1_020_000), RTP_AUDIO_CLOCK_RATE, 160),
            720
        );
        assert_eq!(
            clock.timestamp(Some(1_010_000), RTP_AUDIO_CLOCK_RATE, 160),
            721
        );
    }

    // A legal maximum-size access unit may need a full-MTU cached parameter
    // aggregate too. Include that packet's framing, not only its payload.
    #[test]
    fn maximum_access_unit_fits_with_a_full_parameter_aggregate() {
        let mut parameter_data = Vec::new();
        let parameter_bytes = RTP_PAYLOAD_MTU - 2 - 3 * 2; // HEVC AP header and three lengths.
        for (index, kind) in [H265_VPS_TYPE, H265_SPS_TYPE, H265_PPS_TYPE]
            .into_iter()
            .enumerate()
        {
            let length = parameter_bytes / 3 + usize::from(index < parameter_bytes % 3);
            parameter_data.extend_from_slice(&[0, 0, 0, 1, kind << 1, 1]);
            parameter_data.resize(parameter_data.len() + length - 2, 0xab);
        }
        let mut packetizer = VideoPacketizer::new(VideoCodec::H265, 1);
        let mut batch = Vec::new();
        let mut frame = VideoFrame {
            sequence: 0,
            data: Bytes::from(parameter_data),
            keyframe: false,
            codec: VideoCodec::H265,
            timestamp_us: None,
        };
        packetizer.append_rtp_into(&frame, 0, &mut batch).unwrap();
        assert!(batch.is_empty());
        let mut data = vec![0xab; c220_media_protocol::MAX_PAYLOAD_LEN];
        data[..6].copy_from_slice(&[0, 0, 0, 1, 19 << 1, 1]); // HEVC IDR_W_RADL, layer zero.
        frame.data = Bytes::from(data);
        frame.keyframe = true;
        packetizer.append_rtp_into(&frame, 0, &mut batch).unwrap();
        assert_eq!(
            usize::from(u16::from_be_bytes([batch[2], batch[3]])),
            RTP_HEADER_LEN + RTP_PAYLOAD_MTU
        );
        assert!(batch.len() <= MAX_BATCH_BYTES && batch.capacity() <= MAX_BATCH_BYTES);
    }

    #[test]
    fn parameter_and_packet_amplification_are_bounded_before_allocation() {
        let mut parameter = vec![0xab; MAX_PARAMETER_SET_BYTES + 5];
        parameter[..5].copy_from_slice(&[0, 0, 0, 1, 0x67]);
        let mut codec = CodecState::default();
        assert!(codec.inspect(VideoCodec::H264, &parameter).is_err());
        assert!(codec.sps.is_none());
        // Tiny valid NALs can amplify RTP headers far beyond encoded byte size.
        let data = [0, 0, 1, 2, 1].repeat(MAX_BATCH_BYTES / 18 + 1);
        let frame = VideoFrame {
            sequence: 0,
            data: Bytes::from(data),
            keyframe: false,
            codec: VideoCodec::H265,
            timestamp_us: None,
        };
        let mut packetizer = VideoPacketizer::new(VideoCodec::H265, 1);
        let mut batch = Vec::new();
        assert!(packetizer.append_rtp_into(&frame, 0, &mut batch).is_err());
        assert!(batch.len() <= MAX_BATCH_BYTES && batch.capacity() <= MAX_BATCH_BYTES);
    }
}

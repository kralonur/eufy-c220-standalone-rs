use super::*;
use rtp::codecs::h264::H264Packet;
use rtp::packetizer::Depacketizer;
use std::hint::black_box;
use std::time::Instant;

// Protect FU headers, exact-MTU end bits, sequence wrap and marker placement
// through the actual interleaved batch consumed by an RTSP client.
#[test]
fn packetization_round_trips_fragmented_access_units() {
    for codec in [VideoCodec::H264, VideoCodec::H265] {
        let header: &[u8] = match codec {
            VideoCodec::H264 => &[0x65],
            VideoCodec::H265 => &[0x27, 0x29], // IRAP with non-default layer/TID bits.
        };
        let fragment_body_len = match codec {
            VideoCodec::H264 => RTP_PAYLOAD_MTU - 2,
            VideoCodec::H265 => RTP_PAYLOAD_MTU - H265_FU_HEADER_LEN,
        };
        for body_len in [16, fragment_body_len * 2, fragment_body_len * 2 + 1] {
            let mut nal = header.to_vec();
            nal.extend(std::iter::repeat_n(0xab, body_len));
            let mut data = vec![0, 0, 0, 1];
            data.extend_from_slice(&nal);
            let frame = VideoFrame {
                sequence: 0,
                data: Bytes::from(data),
                codec,
                keyframe: true,
                timestamp_us: Some(1_000_000),
            };
            let mut packetizer = VideoPacketizer::new(codec, 0x1234_5678);
            packetizer.stream.sequence = u16::MAX;
            let mut batch = Vec::new();
            packetizer.append_rtp_into(&frame, 6, &mut batch).unwrap();
            assert_eq!(batch[1], 6);
            let packets = rtp_packets(&batch);
            let mut reconstructed = Vec::new();
            let mut h264 = H264Packet::default();
            for (index, packet) in packets.iter().enumerate() {
                assert!(packet.len() <= RTP_HEADER_LEN + RTP_PAYLOAD_MTU);
                assert_eq!(packet[1] & 0x80 != 0, index + 1 == packets.len());
                assert_eq!(
                    u16::from_be_bytes([packet[2], packet[3]]),
                    u16::MAX.wrapping_add(index as u16)
                );
                let payload = &packet[RTP_HEADER_LEN..];
                match codec {
                    VideoCodec::H264 => reconstructed.extend_from_slice(
                        &h264.depacketize(&Bytes::copy_from_slice(payload)).unwrap(),
                    ),
                    VideoCodec::H265 if (payload[0] >> 1) & 0x3f == 49 => {
                        assert_eq!(payload[0] & 0x81, nal[0] & 0x81);
                        assert_eq!(payload[1], nal[1]);
                        assert_eq!(payload[2] & 0x80 != 0, index == 0);
                        assert_eq!(payload[2] & 0x40 != 0, index + 1 == packets.len());
                        if index == 0 {
                            reconstructed.extend_from_slice(&[
                                (payload[0] & 0x81) | ((payload[2] & 0x3f) << 1),
                                payload[1],
                            ]);
                        }
                        reconstructed.extend_from_slice(&payload[H265_FU_HEADER_LEN..]);
                    }
                    VideoCodec::H265 => reconstructed.extend_from_slice(payload),
                }
            }
            match codec {
                VideoCodec::H264 => assert_eq!(&reconstructed[4..], nal),
                VideoCodec::H265 => assert_eq!(reconstructed, nal),
            }
        }
    }
}

#[test]
fn capture_clock_handles_wrap_regression_and_missing_timestamps() {
    let mut clock = RtpClock::default();
    let mut video =
        |time| clock.timestamp(time, RTP_VIDEO_CLOCK_RATE, RTP_VIDEO_TIMESTAMP_INCREMENT);
    assert_eq!(video(Some(0)), 0);
    let before = video(Some(47_721_858_000));
    let after = video(Some(47_721_924_666));
    assert!(after < before);
    assert_eq!(after.wrapping_sub(before), 5_999);
    assert_eq!(video(Some(1)), after.wrapping_add(1));
    let mut clock = RtpClock::default();
    let mut audio = |time| clock.timestamp(time, RTP_AUDIO_CLOCK_RATE, AUDIO_FRAME_LEN as u32);
    assert_eq!(audio(None), 0);
    assert_eq!(audio(Some(1_000_000)), 160);
    assert_eq!(audio(Some(1_020_000)), 320);
    assert_eq!(audio(None), 480);
    assert_eq!(audio(Some(2_000_000)), 640);
}

// Manual measurement, not a CI timing assertion. Exercise the production batch
// path rather than allocating test packet metadata inside the timed region.
#[test]
#[ignore = "manual performance measurement; run with --release --ignored --nocapture"]
fn benchmark_media_packetization() {
    const ITERATIONS: usize = 2_000;
    for codec in [VideoCodec::H264, VideoCodec::H265] {
        for frame_len in [160, 64 * 1024] {
            let mut data = vec![0, 0, 0, 1];
            match codec {
                VideoCodec::H264 => data.push(0x61),
                VideoCodec::H265 => data.extend_from_slice(&[0x02, 0x01]),
            }
            data.resize(frame_len, 0xab);
            let frame = VideoFrame {
                sequence: 0,
                data: Bytes::from(data),
                codec,
                keyframe: false,
                timestamp_us: Some(1_000_000),
            };
            let mut packetizer = VideoPacketizer::new(codec, 0x1234_5678);
            let mut output =
                Vec::with_capacity(RTP_PAYLOAD_MTU + RTP_HEADER_LEN + INTERLEAVED_HEADER_LEN);
            let start = Instant::now();
            let mut bytes = 0;
            for _ in 0..ITERATIONS {
                output.clear();
                packetizer
                    .append_rtp_into(black_box(&frame), 0, &mut output)
                    .unwrap();
                bytes += black_box(&output).len();
            }
            eprintln!(
                "{codec:?} frame={frame_len} iterations={ITERATIONS} bytes={bytes} elapsed_us={}",
                start.elapsed().as_micros()
            );
        }
    }
}

//! HEVC access-unit inspection.
//!
//! Ported from the legacy C engine, which marks a record
//! as a keyframe when its access unit contains an HEVC IRAP picture.

/// NAL unit types that begin a new group of pictures: BLA_W_LP through
/// RSV_IRAP_VCL23. Types 16..=23 cover BLA, IDR and CRA pictures.
const IRAP_NAL_TYPES: core::ops::RangeInclusive<u8> = 16..=23;

/// Whether an Annex B HEVC access unit contains an IRAP picture.
///
/// Both 3-byte and 4-byte start codes are accepted, matching the reference
/// implementation. Inspect only headers so an IRAP can be recognized without
/// scanning its entire body or building a NAL index.
pub fn detect(data: &[u8]) -> bool {
    c220_media_protocol::annex_b::headers(data)
        .any(|header| header.len() == 2 && IRAP_NAL_TYPES.contains(&((header[0] >> 1) & 0x3f)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an access unit with a 4-byte start code and one NAL header byte.
    fn access_unit(nal_type: u8) -> alloc::vec::Vec<u8> {
        alloc::vec![0, 0, 0, 1, nal_type << 1, 0x80]
    }

    #[test]
    fn accepts_every_irap_nal_type() {
        for nal_type in 16..=23 {
            assert!(detect(&access_unit(nal_type)), "NAL type {nal_type}");
        }
    }

    #[test]
    fn rejects_non_irap_nal_types() {
        for nal_type in [0, 1, 15, 24, 32, 63] {
            assert!(!detect(&access_unit(nal_type)), "NAL type {nal_type}");
        }
    }

    #[test]
    fn accepts_three_byte_start_codes() {
        assert!(detect(&[0x00, 0x00, 0x01, 19 << 1, 0x80]));
    }

    #[test]
    fn finds_an_irap_picture_after_other_nals() {
        let mut unit = access_unit(32);
        unit.extend_from_slice(&access_unit(19));
        assert!(detect(&unit));
    }

    #[test]
    fn ignores_incomplete_start_codes() {
        assert!(!detect(&[]));
        assert!(!detect(&[0, 0, 1]));
        assert!(!detect(&[0, 0, 0, 1]));
        // Start code with no NAL header byte after it.
        assert!(!detect(&[
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x00, 0x00, 0x01
        ]));
    }
}

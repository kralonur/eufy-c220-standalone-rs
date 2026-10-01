//! Shared, allocation-free Annex-B inspection for the daemon and capture engine.

/// Iterate nonempty NAL units after three- or four-byte start codes. Leading
/// bytes before the first prefix are ignored; each body borrows the input.
pub fn nalus(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = start_codes(data).peekable();
    core::iter::from_fn(move || {
        loop {
            let (start, prefix) = starts.next()?;
            let end = starts.peek().map_or(data.len(), |(start, _)| *start);
            let nal = &data[start + prefix..end];
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    })
}

/// Inspect up to two header bytes after each prefix, without scanning the rest
/// of that NAL first. An IRAP near the start can terminate capture's keyframe
/// check immediately, even when its encoded body is several megabytes long.
pub fn headers(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    start_codes(data).map(move |(start, prefix)| {
        let remaining = &data[start + prefix..];
        // Both supported video codecs need at most the two-byte HEVC header.
        &remaining[..remaining.len().min(2)]
    })
}

fn start_codes(data: &[u8]) -> impl Iterator<Item = (usize, usize)> {
    let mut offset = 0;
    core::iter::from_fn(move || {
        // Both prefixes end in 00 00 01. Include the preceding zero when present.
        let remaining = &data[offset..];
        let index = memchr::memmem::find(remaining, &[0, 0, 1])?;
        let (start, length) = if index > 0 && remaining[index - 1] == 0 {
            (offset + index - 1, 4)
        } else {
            (offset + index, 3)
        };
        offset = start + length;
        Some((start, length))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    // Consecutive prefixes and truncated tails must not become NALs or panic.
    #[test]
    fn handles_empty_nals_and_incomplete_prefixes() {
        for data in [&[][..], &[0, 0], &[0, 0, 1], &[0, 0, 0, 1]] {
            assert_eq!(nalus(data).count(), 0);
        }
        let data = [9, 0, 0, 1, 0, 0, 0, 1, 0x67, 0, 0, 1, 0x68, 0, 0];
        assert_eq!(
            nalus(&data).collect::<Vec<_>>(),
            [&[0x67][..], &[0x68, 0, 0]]
        );
    }

    #[test]
    fn header_inspection_and_body_iteration_agree_on_mixed_prefixes() {
        let data = [9, 0, 0, 1, 0x40, 1, 0xab, 0, 0, 0, 1, 0x26, 1, 0xab];
        assert_eq!(
            headers(&data).collect::<Vec<_>>(),
            [&[0x40, 1][..], &[0x26, 1]]
        );
        assert_eq!(
            nalus(&data).collect::<Vec<_>>(),
            [&[0x40, 1, 0xab][..], &[0x26, 1, 0xab]]
        );
    }
}

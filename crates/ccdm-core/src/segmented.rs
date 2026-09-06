//! Segment planning for multi-connection downloads.
//!
//! Splits a known-size file into N byte ranges (inclusive start/end),
//! the same job XDM's `Piece`/`PieceGrabber` machinery does before the
//! adaptive downloaders take over.

/// Split `total_size` bytes into up to `max_segments` contiguous
/// `(start, end)` ranges (both inclusive).
///
/// * Returns an empty vec for `total_size == 0` or `max_segments == 0`.
/// * Never returns more ranges than there are bytes.
/// * Ranges cover `[0, total_size)` exactly once, in order; the first
///   `remainder` segments get one extra byte.
pub fn plan_segments(total_size: u64, max_segments: usize) -> Vec<(u64, u64)> {
    if total_size == 0 || max_segments == 0 {
        return Vec::new();
    }
    let count = (max_segments as u64).min(total_size) as usize;
    let base = total_size / count as u64;
    let remainder = (total_size % count as u64) as usize;

    let mut ranges = Vec::with_capacity(count);
    let mut start = 0u64;
    for i in 0..count {
        let len = base + if i < remainder { 1 } else { 0 };
        let end = start + len - 1;
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_split() {
        assert_eq!(
            plan_segments(100, 4),
            vec![(0, 24), (25, 49), (50, 74), (75, 99)]
        );
    }

    #[test]
    fn remainder_goes_first() {
        // 10 bytes over 3 segments: 4, 3, 3.
        assert_eq!(plan_segments(10, 3), vec![(0, 3), (4, 6), (7, 9)]);
    }

    #[test]
    fn never_more_segments_than_bytes() {
        let r = plan_segments(2, 8);
        assert_eq!(r, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn covers_everything_exactly_once() {
        for total in [1u64, 7, 100, 1024, 9999] {
            for n in [1usize, 2, 8, 32] {
                let r = plan_segments(total, n);
                assert_eq!(r.first().unwrap().0, 0);
                assert_eq!(r.last().unwrap().1, total - 1);
                let covered: u64 = r.iter().map(|(s, e)| e - s + 1).sum();
                assert_eq!(covered, total);
            }
        }
    }

    #[test]
    fn degenerate_inputs() {
        assert!(plan_segments(0, 8).is_empty());
        assert!(plan_segments(100, 0).is_empty());
    }
}

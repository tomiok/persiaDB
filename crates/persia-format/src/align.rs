//! Alignment of on-disk structures (SPEC §4.1: "All structures 8-byte aligned. Offsets are absolute").

use core::num::NonZeroUsize;

use crate::error::to_u64;

/// The alignment of every on-disk structure (SPEC §4.1).
pub const ALIGNMENT: NonZeroUsize = NonZeroUsize::MIN.saturating_add(7);

/// Zero bytes needed after absolute `offset` to reach the next multiple of `align`.
///
/// Alignment is computed on absolute offsets, so a structure encoded into a buffer that will live at
/// `offset` in its file lines up there, not merely within the buffer.
pub fn padding_for(offset: u64, align: NonZeroUsize) -> usize {
    let align = to_u64(align.get());
    let rem = offset % align; // `align` is non-zero
    let pad = if rem == 0 { 0 } else { align - rem }; // rem < align, so no underflow
    // pad < align <= usize::MAX, so the conversion is lossless.
    usize::try_from(pad).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn spec_alignment_is_eight() {
        assert_eq!(ALIGNMENT.get(), 8);
    }

    #[test]
    fn padding_table() {
        for (offset, align, pad) in [
            (0, 8, 0),
            (1, 8, 7),
            (7, 8, 1),
            (8, 8, 0),
            (9, 8, 7),
            (8192, 8, 0),
            (5, 1, 0),
            (5, 3, 1),
            (u64::MAX, 8, 1),
            (u64::MAX, 1, 0),
            (3, usize::MAX, usize::MAX - 3),
        ] {
            assert_eq!(
                padding_for(offset, nz(align)),
                pad,
                "offset={offset} align={align}"
            );
        }
    }
    /// Checks invariant 5 for one pair: `pad` is in `0..align` and `offset + pad` is a multiple of `align`.
    /// The sum is computed in `u128`, so it cannot overflow even at `u64::MAX`.
    fn assert_pads_to_boundary(offset: u64, align: usize) {
        let pad = padding_for(offset, nz(align));
        assert!(pad < align, "pad {pad} >= align {align} (offset={offset})");
        let end = u128::from(offset) + pad as u128;
        assert_eq!(
            end % align as u128,
            0,
            "offset={offset} align={align} pad={pad} does not reach a boundary"
        );
    }

    #[test]
    fn padding_is_below_align_and_reaches_a_multiple() {
        // Exhaustive over small offsets/alignments plus the top of the u64 range (SPEC §4.1).
        let aligns = (1..=17).chain([32, 64, 4096, 1 << 20]);
        for align in aligns {
            for offset in (0..=70).chain(u64::MAX - 70..=u64::MAX) {
                assert_pads_to_boundary(offset, align);
            }
        }
    }

    #[test]
    fn padding_at_extreme_alignments_does_not_overflow() {
        let huge = [usize::MAX, usize::MAX - 1, 1 << (usize::BITS - 1)];
        for align in huge {
            for offset in [0, 1, 2, 7, u64::MAX - 1, u64::MAX] {
                assert_pads_to_boundary(offset, align);
            }
        }
    }

    #[test]
    fn padding_is_zero_exactly_on_boundaries() {
        for align in [1_usize, 2, 8, 4096] {
            for k in 0..8_u64 {
                let boundary = k * align as u64;
                assert_eq!(padding_for(boundary, nz(align)), 0, "boundary {boundary}");
                if align > 1 {
                    // One past a boundary needs the full align - 1 bytes; one before needs exactly one.
                    assert_eq!(padding_for(boundary + 1, nz(align)), align - 1);
                    assert_eq!(padding_for(boundary + align as u64 - 1, nz(align)), 1);
                }
            }
        }
    }
}

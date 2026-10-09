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
}

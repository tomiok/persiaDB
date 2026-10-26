//! One-byte quantization of field lengths, the `Norms` section's values (SPEC §5.3.4, ROADMAP 5.2.4).
//!
//! Lucene's `SmallFloat.intToByte4`: lengths below 24 are stored as is; above, `len − 24` keeps its 4 most
//! significant bits (a 3-bit mantissa after the implicit leading one) and its bit length. Encoding rounds down, so
//! [`decode`]`(`[`encode`]`(n)) ≤ n`, exactly `n` below 40, and otherwise less than `(n − 24) / 8` below it: under
//! 12.5 % low. The code is monotonic, so a smaller byte always means a shorter field (`min_norm`, SPEC §5.3.2).

/// Lengths below this are their own code.
const FREE_VALUES: u32 = 24;
/// Longest length with its own code; longer ones share the last code (no document gets near it, SPEC §6.4.1).
pub(crate) const MAX_EXACT_INPUT: u32 = i32::MAX.unsigned_abs();

/// The byte a field of `len` tokens is stored as.
pub(crate) fn encode(len: u32) -> u8 {
    let len = len.min(MAX_EXACT_INPUT);
    let Some(x) = len.checked_sub(FREE_VALUES) else {
        // len < 24.
        return u8::try_from(len).unwrap_or(u8::MAX);
    };
    let code = FREE_VALUES.saturating_add(int4(x));
    u8::try_from(code).unwrap_or(u8::MAX)
}

/// The length a norm byte stands for: the smallest length that encodes to it.
pub(crate) fn decode(norm: u8) -> u32 {
    let code = u32::from(norm);
    match code.checked_sub(FREE_VALUES) {
        None => code,
        Some(c) => FREE_VALUES.saturating_add(from_int4(c)),
    }
}

/// `SmallFloat.longToInt4`: 4 significant bits of `x`, the leading one implicit, and the shift.
fn int4(x: u32) -> u32 {
    let bits = u32::BITS.saturating_sub(x.leading_zeros());
    if bits < 4 {
        return x;
    }
    let shift = bits.saturating_sub(4);
    ((x >> shift) & 0x07) | (shift.saturating_add(1) << 3)
}

fn from_int4(c: u32) -> u32 {
    let bits = c & 0x07;
    match (c >> 3).checked_sub(1) {
        None => bits,
        Some(shift) => (bits | 0x08).checked_shl(shift).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn exact_below_40() {
        for n in 0..40 {
            assert_eq!(decode(encode(n)), n, "{n}");
        }
        assert_eq!(encode(40), encode(41));
    }

    #[test]
    fn codes_cover_every_byte_monotonically() {
        assert_eq!(encode(0), 0);
        assert_eq!(encode(MAX_EXACT_INPUT), 255);
        assert_eq!(encode(u32::MAX), 255);
        for b in 0..=255_u8 {
            assert_eq!(encode(decode(b)), b, "{b}");
            if b > 0 {
                assert!(decode(b) > decode(b - 1), "{b}");
            }
        }
        // Golden values: the table is part of the format.
        assert_eq!(
            [
                decode(23),
                decode(24),
                decode(40),
                decode(41),
                decode(100),
                decode(255)
            ],
            [23, 24, 40, 42, 3096, 2_013_265_944]
        );
    }

    proptest! {
        #[test]
        fn rounds_down_by_under_an_eighth(n in any::<u32>()) {
            let d = decode(encode(n));
            let n = n.min(MAX_EXACT_INPUT);
            prop_assert!(d <= n);
            if n >= 40 {
                prop_assert!(n - d < (n - FREE_VALUES) / 8 + 1);
                prop_assert!(f64::from(n - d) < f64::from(n) / 8.0);
            }
        }

        #[test]
        fn monotonic(a in any::<u32>(), b in any::<u32>()) {
            let (lo, hi) = (a.min(b), a.max(b));
            prop_assert!(encode(lo) <= encode(hi));
        }
    }
}

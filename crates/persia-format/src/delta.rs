//! Delta coding of strictly increasing `u32` sequences (SPEC §5.2.1 "Delta coding").
//!
//! Values are stored as gaps minus one, `g_i = v_i - v_(i-1) - 1`, where `v_(-1)` is the previous block's last
//! value or, for the first block of a list (`prev = None`), `-1`. Every gap sequence therefore decodes to a
//! strictly increasing sequence; the only invalid input is one that runs past `u32::MAX`.
//!
//! Both directions work in place on a caller-provided buffer: the postings hot path unpacks gaps into a block
//! buffer and turns them into docids without allocating.

use crate::error::{Corruption, EncodeError};

/// Replaces strictly increasing `values` with their gaps, given the value before the first (`None` at the
/// start of a list).
///
/// # Errors
/// [`EncodeError::NotStrictlyIncreasing`] at the first value not greater than its predecessor (or than
/// `prev`); `values` is then unchanged.
pub fn encode_in_place(values: &mut [u32], prev: Option<u32>) -> Result<(), EncodeError> {
    let mut last = prev;
    for (index, &value) in values.iter().enumerate() {
        if last.is_some_and(|last| value <= last) {
            return Err(EncodeError::NotStrictlyIncreasing { index });
        }
        last = Some(value);
    }
    let mut next = first_after(prev);
    for value in values {
        let current = u64::from(*value);
        // Validated above: current >= next, and both fit in u32 + 1, so neither operation wraps.
        *value = u32::try_from(current.wrapping_sub(next)).unwrap_or(u32::MAX);
        next = current.wrapping_add(1);
    }
    Ok(())
}

/// Replaces `gaps` with the values they encode, given the value before the first (`None` at the start of
/// a list).
///
/// # Errors
/// [`Corruption::ValueOverflow`] if a value would exceed `u32::MAX`; `gaps` is then partially decoded. The
/// caller attaches the offset of the enclosing structure.
pub fn decode_in_place(gaps: &mut [u32], prev: Option<u32>) -> Result<(), Corruption> {
    let mut next = first_after(prev);
    for gap in gaps {
        // next <= 2^32 and gap < 2^32: the sum cannot wrap a u64.
        let value = next.wrapping_add(u64::from(*gap));
        *gap = u32::try_from(value).map_err(|_| Corruption::ValueOverflow { bits: u32::BITS })?;
        next = value.wrapping_add(1);
    }
    Ok(())
}

/// The smallest value allowed after `prev`: `prev + 1`, or 0 at the start of a list. Kept in a `u64` because
/// `u32::MAX + 1` is a valid "nothing can follow" bound.
fn first_after(prev: Option<u32>) -> u64 {
    prev.map_or(0, |p| u64::from(p).wrapping_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(values: &[u32], prev: Option<u32>) -> Result<Vec<u32>, EncodeError> {
        let mut buf = values.to_vec();
        encode_in_place(&mut buf, prev).map(|()| buf)
    }

    /// Golden gaps (SPEC §5.2.1): the first gap of a list is the value itself; consecutive ids cost 0.
    #[test]
    fn golden_gaps() {
        assert_eq!(encode(&[0, 1, 2, 3], None), Ok(vec![0, 0, 0, 0]));
        assert_eq!(encode(&[5, 6, 10], None), Ok(vec![5, 0, 3]));
        assert_eq!(encode(&[5, 6, 10], Some(3)), Ok(vec![1, 0, 3]));
        assert_eq!(encode(&[u32::MAX], Some(u32::MAX - 1)), Ok(vec![0]));
        assert_eq!(encode(&[u32::MAX], None), Ok(vec![u32::MAX]));
        assert_eq!(encode(&[0, u32::MAX], None), Ok(vec![0, u32::MAX - 1]));
        assert_eq!(encode(&[], Some(7)), Ok(vec![]));
    }

    #[test]
    fn encode_rejects_non_increasing_input_without_touching_it() {
        let mut values = [1, 4, 4, 9];
        assert_eq!(
            encode_in_place(&mut values, None),
            Err(EncodeError::NotStrictlyIncreasing { index: 2 })
        );
        assert_eq!(values, [1, 4, 4, 9]);
        assert_eq!(
            encode(&[3], Some(3)),
            Err(EncodeError::NotStrictlyIncreasing { index: 0 })
        );
        assert_eq!(
            encode(&[0], Some(u32::MAX)),
            Err(EncodeError::NotStrictlyIncreasing { index: 0 })
        );
        assert_eq!(
            encode(&[9, 2], None),
            Err(EncodeError::NotStrictlyIncreasing { index: 1 })
        );
    }

    #[test]
    fn decode_inverts_encode() {
        let mut gaps = vec![1, 0, 3];
        decode_in_place(&mut gaps, Some(3)).unwrap();
        assert_eq!(gaps, [5, 6, 10]);
        let mut gaps = vec![u32::MAX];
        decode_in_place(&mut gaps, None).unwrap();
        assert_eq!(gaps, [u32::MAX]);
    }

    #[test]
    fn decode_rejects_values_past_u32_max() {
        let overflow = Err(Corruption::ValueOverflow { bits: 32 });
        assert_eq!(decode_in_place(&mut [0], Some(u32::MAX)), overflow);
        assert_eq!(decode_in_place(&mut [u32::MAX, 0], None), overflow);
        assert_eq!(decode_in_place(&mut [1], Some(u32::MAX - 1)), overflow);
        assert_eq!(
            decode_in_place(&mut [u32::MAX, u32::MAX], Some(u32::MAX)),
            overflow
        );
        // Nothing to decode after u32::MAX is fine.
        assert_eq!(decode_in_place(&mut [], Some(u32::MAX)), Ok(()));
    }
}

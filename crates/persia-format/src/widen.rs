//! Lossless integer widenings that `From` does not provide.
//!
//! `u32 -> usize` and `usize -> u64` have no `From` impl because Rust also targets 16-bit platforms
//! and could in principle target 128-bit ones. Persia DB only supports targets whose pointer width is
//! 32 or 64 bits (the assertion below fails the build elsewhere), so on every supported target both
//! conversions are lossless. These helpers hold the only `as` they need, so callers stay free of
//! unchecked casts (CLAUDE.md "checked ops", ROADMAP 0.6.5) and compile to the same code as a bare
//! `as`: no branch on the hot paths (block decode, postings, scoring).

// The two helpers below are lossless only when 32 <= usize::BITS <= 64; a target outside that range
// must fail to build rather than silently truncate offsets and lengths.
const _: () = assert!(usize::BITS >= 32 && usize::BITS <= 64);

/// Widens a `u32` (a count, ordinal or on-disk length) to a `usize` index, losslessly.
///
/// ```
/// assert_eq!(persia_format::widen::usize_from_u32(u32::MAX), 4_294_967_295_usize);
/// ```
#[must_use]
#[inline]
#[allow(clippy::as_conversions)] // lossless: usize::BITS >= 32 is asserted at compile time above
pub const fn usize_from_u32(n: u32) -> usize {
    n as usize
}

/// Widens a `usize` (an in-memory length) to a `u64` size or offset, losslessly.
///
/// ```
/// assert_eq!(persia_format::widen::u64_from_usize(usize::MAX), u64::try_from(usize::MAX).unwrap());
/// ```
#[must_use]
#[inline]
#[allow(clippy::as_conversions)] // lossless: usize::BITS <= 64 is asserted at compile time above
pub const fn u64_from_usize(n: usize) -> u64 {
    n as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widenings_keep_the_value_at_the_edges() {
        assert_eq!(usize_from_u32(0), 0);
        assert_eq!(usize_from_u32(u32::MAX), usize::try_from(u32::MAX).unwrap());
        assert_eq!(u64_from_usize(0), 0);
        assert_eq!(
            u64_from_usize(usize::MAX),
            u64::try_from(usize::MAX).unwrap()
        );
    }
}

//! xxh3 content hashes and routing hashes (SPEC §4.1, §13).
//!
//! Not for integrity (that is CRC32C, [`crate::checksum`]) and not for anything an adversary can choose
//! colliding inputs for (blob ids are SHA-256, ADR-0002). Hashes may be persisted or decide which shard a
//! document lives on, so the variant is fixed: XXH3 with the default secret and seed 0. Changing it is a
//! format change; the golden vectors in the tests pin it.

/// XXH3-64 of `data`.
///
/// ```
/// assert_eq!(persia_format::hash::xxh3_64(b""), 0x2d06_8005_38d3_94c2);
/// ```
pub fn xxh3_64(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

/// XXH3-128 of `data`.
///
/// ```
/// assert_eq!(persia_format::hash::xxh3_128(b""), 0x99aa_06d3_0147_98d8_6001_c324_468d_497f);
/// ```
pub fn xxh3_128(data: &[u8]) -> u128 {
    xxhash_rust::xxh3::xxh3_128(data)
}

/// Incremental XXH3 over data that arrives in pieces; both widths come from the same state.
///
/// Feeding the same bytes in any split gives the same result as [`xxh3_64`] / [`xxh3_128`] over their
/// concatenation.
///
/// ```
/// use persia_format::hash::{Xxh3, xxh3_64, xxh3_128};
///
/// let mut h = Xxh3::new();
/// h.update(b"hello ");
/// h.update(b"world");
/// assert_eq!(h.digest64(), xxh3_64(b"hello world"));
/// assert_eq!(h.digest128(), xxh3_128(b"hello world"));
/// ```
#[derive(Clone, Default)]
pub struct Xxh3 {
    state: xxhash_rust::xxh3::Xxh3Default,
}

impl core::fmt::Debug for Xxh3 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The internal buffer is noise; the running digest identifies the state.
        f.debug_struct("Xxh3")
            .field("digest64", &self.digest64())
            .finish()
    }
}

impl Xxh3 {
    /// A hasher over no bytes.
    pub const fn new() -> Self {
        Self {
            state: xxhash_rust::xxh3::Xxh3Default::new(),
        }
    }

    /// Appends `data`.
    pub fn update(&mut self, data: &[u8]) {
        self.state.update(data);
    }

    /// XXH3-64 of everything appended so far. Does not reset.
    pub fn digest64(&self) -> u64 {
        self.state.digest()
    }

    /// XXH3-128 of everything appended so far. Does not reset.
    pub fn digest128(&self) -> u128 {
        self.state.digest128()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `i * 131 mod 251` for `i < len`: deterministic bytes without long zero runs.
    fn sample(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from(i * 131 % 251).unwrap_or(0))
            .collect()
    }

    // One length per XXH3 code path: empty, 1..=3, 4..=8, 9..=16, 17..=128, 129..=240, one stripe block,
    // and several 1024-byte blocks plus a partial one. Empty-input values are the published
    // reference; the others were produced by `xxhash-rust` 0.8.19 and pin it against a silent change.
    #[rustfmt::skip]
    const GOLDEN: &[(usize, u64, u128)] = &[
        (0,    0x2d06_8005_38d3_94c2, 0x99aa_06d3_0147_98d8_6001_c324_468d_497f),
        (3,    0xc3ab_f7ae_2e25_0b5a, 0x1cef_1b44_9ba1_1392_c3ab_f7ae_2e25_0b5a),
        (8,    0x2c21_27bb_b99b_325e, 0xc37b_9c60_9b81_2e65_b97c_a908_010c_ab60),
        (16,   0xbea4_2f62_eac4_54d0, 0x7bf0_40f5_3be4_8d8c_0deb_840a_dcd7_a040),
        (128,  0x3d41_caa0_b80a_4385, 0xace2_e3f9_8ca7_79b2_f5f6_48d0_2c5e_a199),
        (240,  0x4da2_ab79_21d0_6c5e, 0x79ee_b64c_c139_b65a_64ac_bec0_6479_6cc2),
        (1024, 0x89af_ebe6_14a9_26d7, 0xb42e_c12d_d784_ee70_89af_ebe6_14a9_26d7),
        (4000, 0xa133_6be7_bfd9_28d4, 0x0630_f845_344e_8449_a133_6be7_bfd9_28d4),
    ];

    #[test]
    fn golden_vectors() {
        for &(len, h64, h128) in GOLDEN {
            let data = sample(len);
            assert_eq!(xxh3_64(&data), h64, "xxh3_64 len {len}");
            assert_eq!(xxh3_128(&data), h128, "xxh3_128 len {len}");
        }
    }

    #[test]
    fn empty_stream_matches_one_shot() {
        let h = Xxh3::new();
        assert_eq!(h.digest64(), xxh3_64(b""));
        assert_eq!(h.digest128(), xxh3_128(b""));
        assert_eq!(Xxh3::default().digest64(), h.digest64());
    }

    #[test]
    fn digest_does_not_reset() {
        let mut h = Xxh3::new();
        h.update(b"ab");
        assert_eq!(h.digest64(), xxh3_64(b"ab"));
        h.update(b"c");
        assert_eq!(h.digest64(), xxh3_64(b"abc"));
    }
}

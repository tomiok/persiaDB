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
        // The upstream state (buffer, accumulators) has no `Debug`, and digesting would be work in a `{:?}`.
        f.debug_struct("Xxh3").finish_non_exhaustive()
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

    /// xxhsum's sanity-test buffer (`XSUM_fillTestBuffer`, `cli/xsum_sanity_check.c`): byte `i` is the top
    /// byte of `PRIME32 * PRIME64^i` (wrapping).
    fn sample(len: usize) -> Vec<u8> {
        let mut generator: u64 = 2_654_435_761;
        (0..len)
            .map(|_| {
                let [byte, ..] = generator.to_be_bytes();
                generator = generator.wrapping_mul(11_400_714_785_074_694_797);
                byte
            })
            .collect()
    }

    // Seed-0 vectors from xxHash's tests/sanity_test_vectors.h (Cyan4973/xxHash@680bf46), at the edges of
    // every XXH3 code path: empty, 1..=3, 4..=8, 9..=16, 17..=128, 129..=240, and long inputs (one
    // 1024-byte block; several blocks plus a partial stripe). XXH128 is stored there as {low64, high64}.
    #[rustfmt::skip]
    const GOLDEN: &[(usize, u64, u128)] = &[
        (0,   0x2d06_8005_38d3_94c2, 0x99aa_06d3_0147_98d8_6001_c324_468d_497f),
        (1,   0xc44b_dff4_074e_ecdb, 0xa6cd_5e93_9200_0f6a_c44b_dff4_074e_ecdb),
        (3,   0x5424_7382_a8d6_b94d, 0x20ef_c49f_f024_22ea_5424_7382_a8d6_b94d),
        (4,   0xe5dc_74bc_5184_8a51, 0x970d_585a_c632_bf8e_2e7d_8d68_76a3_9fe9),
        (8,   0x24cc_c9ac_aa9f_65e4, 0x47a7_f080_d82b_b456_64c6_9cab_4bb2_1dc5),
        (9,   0x14d5_001c_15dd_3f2b, 0x564e_f607_8950_d457_ed7c_cbc5_01eb_7501),
        (16,  0x981b_17d3_6c74_98c9, 0xc68c_368e_cf8a_9c05_5629_8025_8a99_8629),
        (17,  0x796f_5acd_3a60_f862, 0x955f_a786_43ed_3669_abbc_12d1_1973_d7db),
        (128, 0xfcff_2412_6754_d861, 0x3999_2220_e045_260a_ebb1_5e34_a7fb_5ab1),
        (129, 0x98f1_b0a6_79a2_ca29, 0x0381_5fc9_1f1b_30b6_86c9_e3bc_8f0a_3b5c),
        (240, 0x81c3_c2b6_7f56_8ccf, 0xaa42_02da_a276_9dc8_5c9a_ae94_c8eb_e5a0),
        (241, 0xc5a6_39ec_d203_0e5e, 0x99a8_0ecf_0ecf_c647_c5a6_39ec_d203_0e5e),
        (1024,0xdd85_c9b5_c110_9c5c, 0x0d30_d240_71c6_4c57_dd85_c9b5_c110_9c5c),
        (2367,0xcb37_aeb9_e5d3_61ed, 0xe89c_0f6f_f369_b427_cb37_aeb9_e5d3_61ed),
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

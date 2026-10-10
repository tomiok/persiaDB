//! Property tests for block codecs (ROADMAP 1.4.1, 1.4.2; SPEC §5.2 "Codecs", ADR-0005).
//!
//! Invariants:
//! 1. Round trip: for every codec and valid level, `decompress_block(compress_block(x))` is `x`, appended
//!    after whatever `out` already held, and the bytes `compress_block` reports equal what it appended.
//! 2. The bomb guard is exact: `max_len == len` decodes, `max_len == len - 1` is `UncompressedTooLarge`.
//! 3. A header that lies about the length is always caught: any `uncompressed_len` other than the true one
//!    is `UncompressedLenMismatch` (or `UncompressedTooLarge` above the limit), never a short or long output.
//! 4. Mutated payloads never panic. Every error is at the caller's offset, is one of the codec errors, and
//!    leaves `out` untouched; every success appends exactly `uncompressed_len` bytes.

// Test-only helpers index, cast and unwrap on purpose (see reader_writer_props.rs).
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

mod common;

use common::{apply, mutation};
use persia_format::compress::{Codec, DEFAULT_ZSTD_LEVEL, compress_block, decompress_block};
use persia_format::{Corruption, Error};
use proptest::prelude::*;

fn codec() -> impl Strategy<Value = Codec> {
    prop_oneof![Just(Codec::None), Just(Codec::Zstd)]
}

/// Mostly compressible data (runs of a small alphabet), sometimes pure noise.
fn data() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => proptest::collection::vec((0_u8..4, 1_usize..40), 0..200)
            .prop_map(|runs| runs.into_iter().flat_map(|(b, n)| std::iter::repeat_n(b'a' + b, n)).collect()),
        1 => proptest::collection::vec(any::<u8>(), 0..2000),
    ]
}

fn level() -> impl Strategy<Value = i32> {
    prop_oneof![
        Just(DEFAULT_ZSTD_LEVEL),
        Just(-5),
        Just(1),
        Just(9),
        Just(19)
    ]
}

fn len(v: &[u8]) -> u64 {
    v.len() as u64
}

proptest! {
    #[test]
    fn round_trip(codec in codec(), level in level(), src in data(), prefix in proptest::collection::vec(any::<u8>(), 0..8)) {
        let mut packed = prefix.clone();
        let n = compress_block(codec, level, &src, &mut packed).unwrap();
        prop_assert_eq!(&packed[..prefix.len()], &prefix[..]);
        prop_assert_eq!(packed.len(), prefix.len() + n);
        let payload = &packed[prefix.len()..];

        let mut out = prefix.clone();
        decompress_block(codec, payload, len(&src), src.len(), 7, &mut out).unwrap();
        prop_assert_eq!(&out[..prefix.len()], &prefix[..]);
        prop_assert_eq!(&out[prefix.len()..], &src[..]);
    }

    #[test]
    fn bomb_guard_is_exact(codec in codec(), src in data().prop_filter("non-empty", |v| !v.is_empty())) {
        let mut packed = Vec::new();
        compress_block(codec, DEFAULT_ZSTD_LEVEL, &src, &mut packed).unwrap();
        let mut out = Vec::new();
        prop_assert_eq!(
            decompress_block(codec, &packed, len(&src), src.len() - 1, 3, &mut out),
            Err(Error::Corrupt {
                offset: 3,
                reason: Corruption::UncompressedTooLarge { len: len(&src), max: len(&src) - 1 },
            })
        );
        prop_assert!(out.is_empty());
    }

    #[test]
    fn lying_length_is_caught(codec in codec(), src in data(), claimed in 0_u64..4000, max_len in 0_usize..4000) {
        prop_assume!(claimed != len(&src));
        let mut packed = Vec::new();
        compress_block(codec, DEFAULT_ZSTD_LEVEL, &src, &mut packed).unwrap();
        let mut out = vec![0xee];
        let reason = match decompress_block(codec, &packed, claimed, max_len, 11, &mut out) {
            Err(Error::Corrupt { offset: 11, reason }) => reason,
            other => return Err(TestCaseError::fail(format!("{other:?}"))),
        };
        let expected = if claimed > max_len as u64 {
            Corruption::UncompressedTooLarge { len: claimed, max: max_len as u64 }
        } else {
            Corruption::UncompressedLenMismatch {
                expected: claimed,
                actual: if codec == Codec::None { len(&packed) } else { len(&src) },
            }
        };
        prop_assert_eq!(reason, expected);
        prop_assert_eq!(out, [0xee]);
    }

    #[test]
    fn mutated_payloads_never_panic(
        codec in codec(),
        src in data(),
        m in mutation(),
        claimed_delta in prop_oneof![4 => Just(0_i64), 1 => -3_i64..3],
        offset in any::<u64>(),
    ) {
        let mut packed = Vec::new();
        compress_block(codec, DEFAULT_ZSTD_LEVEL, &src, &mut packed).unwrap();
        let packed = apply(packed, &m);
        let claimed = len(&src).saturating_add_signed(claimed_delta);
        let mut out = vec![1, 2, 3];
        match decompress_block(codec, &packed, claimed, 4096, offset, &mut out) {
            Ok(()) => prop_assert_eq!(len(&out), 3 + claimed),
            Err(Error::Corrupt { offset: at, reason }) => {
                prop_assert_eq!(at, offset);
                prop_assert!(
                    matches!(
                        reason,
                        Corruption::UncompressedTooLarge { .. }
                            | Corruption::UncompressedLenMismatch { .. }
                            | Corruption::InvalidCompressedData
                    ),
                    "{reason:?}"
                );
                prop_assert_eq!(out, [1, 2, 3]);
            }
            Err(other) => prop_assert!(false, "decompression returns only Error::Corrupt: {other:?}"),
        }
    }
}

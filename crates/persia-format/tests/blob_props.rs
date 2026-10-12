//! Property tests for `BlobId` and `BlobRef` (ROADMAP 1.10.1; SPEC §9.1, ADR-0011): round-trip, canonical
//! encoding, and decoding of arbitrary or mutated bytes never panics and accepts only canonical input. Golden
//! encodings and per-field error offsets live in the `src/blob.rs` unit tests.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use persia_format::blob::ParseBlobIdError;
use persia_format::{BlobId, BlobRef, Error, Reader, Writer};
use proptest::prelude::*;

fn blob_id() -> impl Strategy<Value = BlobId> {
    any::<[u8; 32]>().prop_map(BlobId::from_bytes)
}

/// Valid references, with sizes and chunk sizes at the edges of the chunk-count rule.
fn blob_ref() -> impl Strategy<Value = BlobRef> {
    let chunk_size = prop_oneof![
        Just(1_u32),
        Just(BlobRef::MAX_CHUNK_SIZE),
        1..=BlobRef::MAX_CHUNK_SIZE
    ];
    let mime = proptest::string::string_regex("[ -~]{1,255}").unwrap();
    let key = proptest::string::string_regex(".{1,32}")
        .unwrap()
        .prop_filter("key length", |k| k.len() <= BlobRef::MAX_META_KEY_LEN);
    let value = proptest::string::string_regex(".{0,64}")
        .unwrap()
        .prop_filter("value length", |v| v.len() <= BlobRef::MAX_META_VALUE_LEN);
    let meta = proptest::collection::btree_map(key, value, 0..=BlobRef::MAX_META_ENTRIES);
    (blob_id(), chunk_size, any::<u64>(), mime, meta).prop_map(
        |(id, chunk_size, raw, mime, meta): (_, u32, u64, String, BTreeMap<String, String>)| {
            let max_size = u64::from(chunk_size) * u64::from(u32::MAX);
            let size = match raw % 4 {
                0 => max_size,
                1 => raw % 1024,
                _ => raw % (max_size + 1),
            };
            BlobRef::new(id, size, mime, chunk_size, meta).unwrap()
        },
    )
}

fn encode(b: &BlobRef) -> Vec<u8> {
    let mut w = Writer::new();
    b.encode(&mut w);
    w.into_inner()
}

/// Decodes `bytes`; a success must be the canonical encoding of the value, a failure must not move the reader.
fn check_decode(bytes: &[u8]) -> Result<BlobRef, Error> {
    let mut r = Reader::new(bytes);
    match BlobRef::decode(&mut r) {
        Ok(b) => {
            assert_eq!(
                &bytes[..r.position()],
                encode(&b),
                "non-canonical input accepted"
            );
            Ok(b)
        }
        Err(e) => {
            assert_eq!(r.position(), 0);
            assert!(
                e.is_corruption() || matches!(e, Error::Unsupported { .. }),
                "{e:?}"
            );
            Err(e)
        }
    }
}

proptest! {
    #[test]
    fn blob_ref_round_trips(b in blob_ref(), tail in proptest::collection::vec(any::<u8>(), 0..8)) {
        let mut bytes = encode(&b);
        prop_assert_eq!(bytes.len(), b.encoded_len());
        prop_assert!(bytes.len() <= BlobRef::MAX_ENCODED_LEN);
        let len = bytes.len();
        bytes.extend(tail);
        let mut r = Reader::new(&bytes);
        prop_assert_eq!(BlobRef::decode(&mut r).unwrap(), b.clone());
        prop_assert_eq!(r.position(), len);
        let rebuilt = BlobRef::new(b.blob_id(), b.size(), b.mime(), b.chunk_size(), b.user_meta().clone());
        prop_assert_eq!(rebuilt.unwrap(), b);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
        let _ = check_decode(&bytes);
    }

    #[test]
    fn mutated_encodings_decode_canonically_or_fail(
        b in blob_ref(),
        flips in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
    ) {
        let mut bytes = encode(&b);
        for (at, byte) in flips {
            let i = at.index(bytes.len());
            bytes[i] = byte;
        }
        let _ = check_decode(&bytes);
    }

    #[test]
    fn blob_id_hex_round_trips(id in blob_id()) {
        let hex = id.to_string();
        prop_assert_eq!(hex.len(), BlobId::HEX_LEN);
        prop_assert!(hex.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        prop_assert_eq!(hex.parse::<BlobId>(), Ok(id));
    }

    #[test]
    fn blob_id_parse_rejects_anything_else(s in ".{0,70}") {
        let canonical = s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'));
        match s.parse::<BlobId>() {
            Ok(id) => {
                prop_assert!(canonical);
                prop_assert_eq!(id.to_string(), s);
            }
            Err(ParseBlobIdError::Length { len }) => prop_assert_eq!(len, s.len()),
            Err(ParseBlobIdError::Digit { index }) => {
                prop_assert!(!canonical);
                prop_assert!(!matches!(s.as_bytes()[index], b'0'..=b'9' | b'a'..=b'f'));
                prop_assert!(s.as_bytes()[..index].iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
            }
        }
    }
}

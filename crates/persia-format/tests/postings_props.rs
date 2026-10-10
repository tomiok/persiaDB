//! Property tests for postings blocks (ROADMAP 1.2.3): whole posting lists split into full blocks and a tail
//! round-trip with docids chained across blocks; skipping agrees with decoding; `advance_to` agrees with a
//! linear scan.

// Test-only helpers index and unwrap on purpose (see reader_writer_props.rs).
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::BTreeSet;

use persia_format::bitpack::BLOCK_LEN;
use persia_format::postings::advance_to;
use persia_format::{Reader, Writer};
use proptest::prelude::*;

/// Posting lists: sparse, dense runs, and lists ending at `u32::MAX`; lengths cross several block boundaries.
fn posting_list() -> impl Strategy<Value = Vec<u32>> {
    prop_oneof![
        proptest::collection::btree_set(any::<u32>(), 0..400)
            .prop_map(|s: BTreeSet<u32>| s.into_iter().collect()),
        proptest::collection::btree_set(0_u32..2000, 0..400)
            .prop_map(|s: BTreeSet<u32>| s.into_iter().collect()),
        (0_u32..400).prop_map(|n| (0..n).map(|i| u32::MAX - n + 1 + i).collect()),
    ]
}

/// Encodes a list as SPEC §5.2.1 describes: `n / 128` full blocks, then a tail of `n % 128` if non-empty.
fn encode(docids: &[u32]) -> Vec<u8> {
    let mut w = Writer::new();
    let mut prev = None;
    let (blocks, tail) = docids.as_chunks::<BLOCK_LEN>();
    for block in blocks {
        w.write_postings_block(block, prev, block[0] % 7).unwrap();
        prev = block.last().copied();
    }
    if !tail.is_empty() {
        w.write_postings_tail(tail, prev, 5).unwrap();
    }
    w.into_inner()
}

proptest! {
    #[test]
    fn lists_round_trip(docids in posting_list()) {
        let bytes = encode(&docids);
        let mut r = Reader::new(&bytes);
        let mut out = [0; BLOCK_LEN];
        let mut decoded = Vec::new();
        let mut prev = None;
        for block in docids.as_chunks::<BLOCK_LEN>().0 {
            let header = r.read_postings_block(prev, &mut out).unwrap();
            prop_assert_eq!(header.max_tf, block[0] % 7);
            decoded.extend_from_slice(&out);
            prev = Some(header.last_docid);
        }
        if docids.len() % BLOCK_LEN != 0 {
            let tail = r.read_postings_tail(prev, &mut out).unwrap();
            prop_assert_eq!(tail.count, docids.len() % BLOCK_LEN);
            prop_assert_eq!(tail.max_tf, 5);
            decoded.extend_from_slice(&out[..tail.count]);
        }
        prop_assert!(r.is_empty());
        prop_assert_eq!(decoded, docids);
    }

    #[test]
    fn skipping_full_blocks_lands_where_decoding_does(docids in posting_list()) {
        let bytes = encode(&docids);
        let (mut skip, mut read) = (Reader::new(&bytes), Reader::new(&bytes));
        let mut out = [0; BLOCK_LEN];
        let mut prev = None;
        for _ in 0..docids.len() / BLOCK_LEN {
            let skipped = skip.skip_postings_block().unwrap();
            prop_assert_eq!(read.read_postings_block(prev, &mut out).unwrap(), skipped);
            prop_assert_eq!(skip.position(), read.position());
            prev = Some(skipped.last_docid);
        }
    }

    #[test]
    fn advance_to_matches_linear_scan(
        docids in proptest::collection::btree_set(0_u32..1000, 0..=128).prop_map(|s| s.into_iter().collect::<Vec<_>>()),
        from in 0_usize..140,
        target in 0_u32..1002,
    ) {
        let expected = (from..docids.len()).find(|&i| docids[i] >= target).unwrap_or(docids.len());
        prop_assert_eq!(advance_to(&docids, from, target), expected);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..700), prev in proptest::option::of(any::<u32>())) {
        let mut out = [0; BLOCK_LEN];
        let mut r = Reader::new(&bytes);
        if let Ok(header) = r.read_postings_block(prev, &mut out) {
            prop_assert_eq!(out[BLOCK_LEN - 1], header.last_docid);
            prop_assert!(out.windows(2).all(|w| w[0] < w[1]));
        }
        let mut r = Reader::new(&bytes);
        if let Ok(tail) = r.read_postings_tail(prev, &mut out) {
            prop_assert!(out[..tail.count].windows(2).all(|w| w[0] < w[1]));
        }
    }
}

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
        w.write_postings_block(block, prev).unwrap();
        prev = block.last().copied();
    }
    if !tail.is_empty() {
        w.write_postings_tail(tail, prev).unwrap();
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
        for _ in 0..docids.len() / BLOCK_LEN {
            let header = r.read_postings_block(prev, &mut out).unwrap();
            decoded.extend_from_slice(&out);
            prev = Some(header.last_docid);
        }
        if docids.len() % BLOCK_LEN != 0 {
            let count = r.read_postings_tail(prev, &mut out).unwrap();
            prop_assert_eq!(count, docids.len() % BLOCK_LEN);
            decoded.extend_from_slice(&out[..count]);
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
            let skipped = skip.skip_postings_block(prev).unwrap();
            prop_assert_eq!(read.read_postings_block(prev, &mut out).unwrap(), skipped);
            prop_assert_eq!(skip.position(), read.position());
            prev = Some(skipped.last_docid);
        }
    }

    /// Whatever a corrupted list holds and however blocks are skipped or decoded, every docid a reader
    /// accepts (decoded docids and skipped blocks' `last_docid`) is strictly greater than the one before.
    #[test]
    fn skip_read_mix_over_corrupted_lists_never_goes_backwards(
        docids in posting_list(),
        flip in any::<(prop::sample::Index, u8)>(),
        pattern in any::<u64>(),
    ) {
        let mut bytes = encode(&docids);
        if !bytes.is_empty() {
            let i = flip.0.index(bytes.len());
            bytes[i] ^= flip.1.max(1);
        }
        let mut r = Reader::new(&bytes);
        let mut out = [0; BLOCK_LEN];
        let mut prev: Option<u32> = None;
        for step in 0..docids.len() / BLOCK_LEN {
            let accepted: Vec<u32> = if pattern >> (step % 64) & 1 == 0 {
                match r.skip_postings_block(prev) {
                    Ok(h) => vec![h.last_docid],
                    Err(_) => break,
                }
            } else {
                match r.read_postings_block(prev, &mut out) {
                    Ok(_) => out.to_vec(),
                    Err(_) => break,
                }
            };
            for d in accepted {
                prop_assert!(prev.is_none_or(|p| d > p), "{d} after {prev:?}");
                prev = Some(d);
            }
        }
        if let Ok(count) = r.read_postings_tail(prev, &mut out) {
            for &d in &out[..count] {
                prop_assert!(prev.is_none_or(|p| d > p));
                prev = Some(d);
            }
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
        if let Ok(count) = r.read_postings_tail(prev, &mut out) {
            prop_assert!(out[..count].windows(2).all(|w| w[0] < w[1]));
        }
    }
}

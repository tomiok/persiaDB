//! Fuzz postings block decoding (ROADMAP 1.2.3): arbitrary bytes never panic; a failed read moves nothing; skipped
//! blocks advance `prev` by at least 128;
//! accepted blocks decode to strictly increasing docids after `prev` that re-encode to the same bytes
//! (for full blocks, when the width is minimal: readers do not reject wider ones, SPEC §5.2.1).
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::bitpack::{BLOCK_LEN, BitWidth};
use persia_format::{Reader, Writer};

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);
    let mut prev = None;
    let mut out = [0_u32; BLOCK_LEN];
    // Each step: a selector byte picks a full block (decoded or skipped) or a tail block, then the block itself.
    while let Ok(selector) = r.read_u8() {
        let start = r.position();
        if selector % 3 == 2 {
            let Ok(header) = r.skip_postings_block(prev) else {
                assert_eq!(r.position(), start);
                break;
            };
            assert!(
                u64::from(header.last_docid) >= prev.map_or(127, |p| u64::from(p) + 128),
                "skip went backwards"
            );
            prev = Some(header.last_docid);
            continue;
        }
        let mut w = Writer::new();
        let decoded = if selector % 3 == 0 {
            let Ok(header) = r.read_postings_block(prev, &mut out) else {
                assert_eq!(r.position(), start);
                break;
            };
            assert_eq!(out[BLOCK_LEN - 1], header.last_docid);
            w.write_postings_block(&out, prev)
                .expect("decoded block re-encodes");
            if header.width != BitWidth::of(&gaps(&out, prev)) {
                w = Writer::new(); // non-minimal width: no byte-for-byte comparison
            }
            &out[..]
        } else {
            let Ok(count) = r.read_postings_tail(prev, &mut out) else {
                assert_eq!(r.position(), start);
                break;
            };
            w.write_postings_tail(&out[..count], prev)
                .expect("decoded tail re-encodes");
            &out[..count]
        };
        assert!(
            decoded.windows(2).all(|p| p[0] < p[1]),
            "not strictly increasing"
        );
        assert!(prev.is_none_or(|p| decoded[0] > p), "does not follow prev");
        if !w.is_empty() {
            assert_eq!(
                w.as_slice(),
                &data[start..r.position()],
                "non-canonical bytes accepted"
            );
        }
        prev = decoded.last().copied();
    }
});

fn gaps(docids: &[u32; BLOCK_LEN], prev: Option<u32>) -> [u32; BLOCK_LEN] {
    let mut gaps = *docids;
    persia_format::delta::encode_in_place(&mut gaps, prev).expect("decoded docids increase");
    gaps
}

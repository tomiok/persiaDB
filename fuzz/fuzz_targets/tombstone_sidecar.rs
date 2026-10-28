//! Fuzz the tombstone sidecar encoding (ROADMAP 6.4.2; SPEC §8.3, ADR-0030): arbitrary bytes never panic, and an
//! accepted sidecar survives encode → decode unchanged, re-encoding to the same bytes the second time. The first
//! encoding may differ from the input only in what the portable roaring format leaves free (container offsets, the
//! choice of container kinds).
//!
//! The first input byte picks the mode: odd, the rest is decoded as it is; even, its CRC32C trailer is recomputed
//! first, so mutations reach the header rules and the roaring parser instead of stopping at the checksum.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::tombstone::TombstoneSidecar;
use persia_format::checksum::crc32c;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let mut input = rest.to_vec();
    if mode % 2 == 0
        && let Some(end) = input.len().checked_sub(4)
    {
        let crc = crc32c(&input[..end]);
        input[end..].copy_from_slice(&crc.to_le_bytes());
    }
    let Ok(sidecar) = TombstoneSidecar::decode(&input) else {
        return;
    };
    let bytes = sidecar.encode();
    let again = TombstoneSidecar::decode(&bytes).expect("an encoded sidecar decodes");
    assert_eq!(again.deleted(), sidecar.deleted());
    assert_eq!(
        (again.segment_id(), again.version(), again.doc_count()),
        (sidecar.segment_id(), sidecar.version(), sidecar.doc_count())
    );
    assert_eq!(again.encode(), bytes);
});

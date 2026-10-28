//! Fuzz the tombstone sidecar encoding (ROADMAP 6.4.2; SPEC §8.3, ADR-0030): arbitrary bytes never panic, and an
//! accepted sidecar survives encode → decode unchanged, re-encoding to the same bytes the second time. The first
//! encoding may differ from the input only in what the portable roaring format leaves free (container offsets, the
//! choice of container kinds).
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::tombstone::TombstoneSidecar;

fuzz_target!(|data: &[u8]| {
    let Ok(sidecar) = TombstoneSidecar::decode(data) else {
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

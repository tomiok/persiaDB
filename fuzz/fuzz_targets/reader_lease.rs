//! Fuzz the reader lease record decoder (ROADMAP 9.2.16; SPEC §9.3, ADR-0054): the writer reads reader lease files
//! other processes wrote, torn or hostile ones included. Arbitrary input never panics, and a record that decodes is
//! exactly the bytes its value encodes (one encoding per value), so a damaged record is never read as another one.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_blob::pack::lease::LeaseRecord;

fuzz_target!(|data: &[u8]| {
    let Ok(record) = LeaseRecord::decode(data) else {
        return;
    };
    assert_eq!(&record.encode()[..], data, "a decoded record is canonical");
    assert_eq!(record.is_opening(), record.manifest_seq().is_none());
});

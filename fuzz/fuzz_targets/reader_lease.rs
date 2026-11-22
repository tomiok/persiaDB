//! Fuzz the reader lease record decoder (ROADMAP 9.2.16; SPEC §9.3, ADR-0054): the writer reads reader lease files
//! other processes wrote, torn or hostile ones included. Arbitrary input never panics, and a record that decodes is
//! exactly the bytes its value encodes (one encoding per value), so a damaged record is never read as another one.
//! The same bytes as a directory entry's name: only `<32 lowercase hex digits>.lease` is a reader lease, and its id
//! names the file back exactly.
#![no_main]

use std::ffi::OsStr;

use libfuzzer_sys::fuzz_target;
use persia_blob::pack::lease::{LEASE_READ_MAX, LeaseRecord, lease_file_name, parse_lease_name};

fuzz_target!(|data: &[u8]| {
    // The writer reads at most `LEASE_READ_MAX` bytes of a file.
    let bounded = &data[..data.len().min(LEASE_READ_MAX as usize)];
    if let Ok(record) = LeaseRecord::decode(bounded) {
        assert_eq!(
            &record.encode()[..],
            bounded,
            "a decoded record is canonical"
        );
        assert_eq!(record.is_opening(), record.manifest_seq().is_none());
        assert_eq!(record.is_opening(), record.db_uuid().is_none());
    }
    if let Ok(name) = std::str::from_utf8(data)
        && let Some(id) = parse_lease_name(OsStr::new(name))
    {
        assert_eq!(lease_file_name(id), name, "a lease name is canonical");
    }
});

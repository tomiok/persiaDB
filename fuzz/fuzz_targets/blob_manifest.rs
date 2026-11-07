//! Fuzz the blob manifest encoding (ROADMAP 9.1.3; SPEC §9.1.1, ADR-0035): arbitrary bytes never panic, and an
//! accepted manifest re-encodes to exactly the input (each value has one encoding).
//!
//! The first input byte picks the mode: odd, the rest is decoded as it is; even, its CRC32C trailer is recomputed
//! first, so mutations reach the header and length rules instead of stopping at the checksum.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_blob::BlobManifest;
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
    let Ok(manifest) = BlobManifest::decode(&input) else {
        return;
    };
    assert_eq!(
        manifest.encode(),
        input,
        "a decoded manifest re-encodes to its input"
    );
    assert_eq!(manifest.encoded_len(), input.len());
    assert_eq!(manifest.chunk_crcs().len(), manifest.chunk_count() as usize);
});

//! Fuzz `BlobRef` decoding (ROADMAP 1.10.1; SPEC §9.1, ADR-0011): arbitrary bytes never panic, a failed decode
//! moves nothing and is corruption or an unsupported version, and an accepted reference re-encodes to exactly the
//! bytes it was read from (one encoding per value) and rebuilds through `BlobRef::new`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::{BlobRef, Error, Reader, Writer};

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::with_base_offset(data, 4096);
    match BlobRef::decode(&mut r) {
        Ok(b) => {
            let mut w = Writer::new();
            b.encode(&mut w);
            assert_eq!(w.as_slice(), &data[..r.position()], "non-canonical input accepted");
            assert_eq!(w.len(), b.encoded_len());
            assert!(w.len() <= BlobRef::MAX_ENCODED_LEN);
            let rebuilt = BlobRef::new(b.blob_id(), b.size(), b.mime(), b.chunk_size(), b.user_meta().clone());
            assert_eq!(rebuilt.as_ref(), Ok(&b));
        }
        Err(e) => {
            assert_eq!(r.position(), 0, "a failed decode moves nothing");
            assert!(e.is_corruption() || matches!(e, Error::Unsupported { .. }), "{e:?}");
        }
    }
});

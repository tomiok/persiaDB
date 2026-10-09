//! Fuzz varint decoding (ROADMAP 1.1.2.2): arbitrary bytes never panic, accepted input is exactly the
//! canonical encoding of the decoded value, and u32/u64 decoders agree.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::{Reader, Writer};

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);
    loop {
        let before = r.position();
        let narrow = Reader::new(r.rest()).read_varint_u32();
        let Ok(value) = r.read_varint_u64() else {
            assert_eq!(r.position(), before, "a failed read must not move the cursor");
            assert!(narrow.is_err(), "u32 accepted what u64 rejected");
            break;
        };
        let consumed = &data[before..r.position()];
        let mut w = Writer::new();
        w.write_varint_u64(value);
        assert_eq!(w.as_slice(), consumed, "accepted a non-canonical encoding");
        assert_eq!(narrow.ok(), u32::try_from(value).ok(), "u32 and u64 decoders disagree");
    }
});

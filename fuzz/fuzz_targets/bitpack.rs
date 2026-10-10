//! Fuzz bit-packed block decoding (ROADMAP 1.2.2.4): a width byte and arbitrary bytes never panic, a failed
//! read moves nothing, and an accepted block re-packs to exactly the bytes it was read from.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::bitpack::BLOCK_LEN;
use persia_format::{Reader, Writer};

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);
    while !r.is_empty() {
        let before = r.position();
        let Ok(width) = r.read_bit_width() else {
            assert_eq!(r.position(), before, "a failed width read must not move the cursor");
            break;
        };
        let start = r.position();
        let mut out = [0_u32; BLOCK_LEN];
        if r.read_packed_block(width, &mut out).is_err() {
            assert_eq!(r.position(), start, "a failed block read must not move the cursor");
            assert_eq!(out, [0; BLOCK_LEN], "a failed block read must not touch the output");
            break;
        }
        let mut w = Writer::new();
        w.write_packed_block(&out, width).expect("decoded values fit their width");
        assert_eq!(w.as_slice(), &data[start..r.position()], "packing is not a bijection");
    }
});

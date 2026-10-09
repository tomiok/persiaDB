//! Fuzz `Reader` (ROADMAP 1.1.1.3): arbitrary bytes and arbitrary op sequences must never panic, and every
//! op must keep the cursor invariants. Input layout: `[base_byte, n_ops, ops..., buffer...]`.
#![no_main]

use std::num::NonZeroUsize;

use libfuzzer_sys::fuzz_target;
use persia_format::Reader;

fuzz_target!(|data: &[u8]| {
    let [base_byte, n_ops, rest @ ..] = data else { return };
    let (ops, buf) = rest.split_at(usize::from(*n_ops).min(rest.len()));
    // Bases near u64::MAX too: offsets must saturate, never overflow.
    let base = if base_byte & 1 == 0 { u64::from(*base_byte) << 8 } else { u64::MAX - u64::from(*base_byte) };
    let mut r = Reader::with_base_offset(buf, base);

    for &op in ops {
        let before = r.position();
        // op = [n:4][huge:1][kind:3]. `huge` turns n into a corrupt-length-field-sized value.
        let small = usize::from(op >> 4);
        let n = if op & 8 == 0 { small } else { usize::MAX - small };
        let ok = match op & 7 {
            0 => r.read_bytes(n).is_ok(),
            1 => r.read_u64().is_ok(),
            2 => r.read_u128().is_ok(),
            3 => r.skip(n).is_ok(),
            4 => r.expect_zeros(n).is_ok(),
            5 => r.skip_padding(NonZeroUsize::new(small + 1).unwrap()).is_ok(),
            6 => r
                .sub_reader(n)
                .map(|mut s| {
                    while s.read_u8().is_ok() {}
                    assert!(s.offset() <= base.saturating_add(buf.len() as u64));
                })
                .is_ok(),
            _ => r.read_array::<3>().is_ok(),
        };
        assert!(r.position() <= buf.len());
        assert_eq!(r.remaining(), buf.len() - r.position());
        assert_eq!(r.offset(), base.saturating_add(r.position() as u64));
        if !ok {
            assert_eq!(r.position(), before, "a failed read must not move the cursor");
        }
    }
});

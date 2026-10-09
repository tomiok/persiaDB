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
        let n = usize::from(op >> 3); // 0..32; huge lengths are covered below
        let ok = match op & 7 {
            0 => r.read_bytes(n).is_ok(),
            1 => r.read_u64().is_ok(),
            2 => r.read_u128().is_ok(),
            3 => r.skip(if op & 8 == 0 { n } else { usize::MAX - n }).is_ok(),
            4 => r.expect_zeros(n).is_ok(),
            5 => r.skip_padding(NonZeroUsize::new(n + 1).unwrap()).is_ok(),
            6 => r.sub_reader(n).map(|mut s| while s.read_u8().is_ok() {}).is_ok(),
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

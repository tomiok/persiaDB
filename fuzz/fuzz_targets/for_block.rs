//! Fuzz FOR block decoding (ROADMAP 1.2.4): arbitrary bytes never panic, a failed read moves nothing, accepted
//! blocks give the same values by `get`, `iter` and `decode_into`, and re-encoding them decodes to the same values (and to
//! the same bytes when the block was canonical: true minimum, minimal width).
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::{Reader, Writer};

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);
    while !r.is_empty() {
        let start = r.position();
        let Ok(block) = r.read_for_block() else {
            assert_eq!(
                r.position(),
                start,
                "a failed read must not move the cursor"
            );
            break;
        };
        let values: Vec<u64> = block.iter().collect();
        assert_eq!(values.len(), block.len());
        for (i, &v) in values.iter().enumerate() {
            assert_eq!(block.get(i), Some(v));
            assert!(v >= block.min());
        }
        assert_eq!(block.get(values.len()), None);
        let mut decoded = vec![1];
        block.decode_into(&mut decoded);
        assert_eq!(
            decoded.get(1..),
            Some(values.as_slice()),
            "decode_into disagrees with iter"
        );

        let mut w = Writer::new();
        w.write_for_block(&values)
            .expect("decoded block re-encodes");
        let again: Vec<u64> = Reader::new(w.as_slice())
            .read_for_block()
            .expect("re-encoded block decodes")
            .iter()
            .collect();
        assert_eq!(again, values);
        let true_min = values.iter().copied().min().unwrap_or(0);
        let minimal = values
            .iter()
            .map(|v| v - true_min)
            .max()
            .unwrap_or(0)
            .bit_width();
        if block.min() == true_min && u32::from(block.width()) == minimal {
            assert_eq!(
                w.as_slice(),
                &data[start..r.position()],
                "canonical block re-encodes differently"
            );
        }
    }
});

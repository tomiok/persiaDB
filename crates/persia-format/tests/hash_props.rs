//! Property tests for xxh3 (ROADMAP 1.3.2): streaming over any split equals one-shot, for both widths.

// Test-only helpers index on purpose (see reader_writer_props.rs).
#![allow(clippy::indexing_slicing)]

use persia_format::hash::{Xxh3, xxh3_64, xxh3_128};
use proptest::prelude::*;
use proptest::sample::Index;

proptest! {
    #[test]
    fn streaming_matches_one_shot(data in proptest::collection::vec(any::<u8>(), 0..5000),
                                  cuts in proptest::collection::vec(any::<Index>(), 0..8)) {
        let mut cuts: Vec<usize> = cuts.iter().map(|c| c.index(data.len() + 1)).collect();
        cuts.sort_unstable();
        let mut h = Xxh3::new();
        let mut start = 0;
        for cut in cuts.into_iter().chain([data.len()]) {
            h.update(&data[start..cut]);
            start = cut;
        }
        prop_assert_eq!(h.digest64(), xxh3_64(&data));
        prop_assert_eq!(h.digest128(), xxh3_128(&data));
    }
}

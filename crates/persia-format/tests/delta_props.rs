//! Property tests for delta coding (ROADMAP 1.2.1.2): round-trip for random sorted sets including empty,
//! single and maximal values, chained blocks, and rejection of non-increasing input.

// Test-only helpers index on purpose (see reader_writer_props.rs).
#![allow(clippy::indexing_slicing)]

use std::collections::BTreeSet;

use persia_format::delta::{decode_in_place, encode_in_place};
use persia_format::{Corruption, EncodeError};
use proptest::prelude::*;

/// Sorted, deduplicated sets that hit the edges: empty, singletons, dense runs, and values at `u32::MAX`.
fn sorted_set() -> impl Strategy<Value = Vec<u32>> {
    let value = prop_oneof![
        any::<u32>(),
        0_u32..300,
        (0_u32..300).prop_map(|d| u32::MAX - d),
    ];
    prop_oneof![
        Just(vec![]),
        Just(vec![u32::MAX]),
        Just(vec![0]),
        proptest::collection::btree_set(value, 0..300)
            .prop_map(|s: BTreeSet<u32>| s.into_iter().collect()),
        (any::<u32>(), 1_usize..300).prop_map(|(start, n)| {
            (0..n)
                .filter_map(|i| start.checked_add(u32::try_from(i).ok()?))
                .collect()
        }),
    ]
}

proptest! {
    #[test]
    fn round_trips_with_and_without_prev(values in sorted_set(), cut in any::<prop::sample::Index>()) {
        // Whole list from the start.
        let mut buf = values.clone();
        encode_in_place(&mut buf, None).unwrap();
        decode_in_place(&mut buf, None).unwrap();
        prop_assert_eq!(&buf, &values);

        // Split into two blocks: the second is coded relative to the first one's last value.
        if !values.is_empty() {
            let cut = cut.index(values.len());
            let (head, tail) = values.split_at(cut);
            let prev = head.last().copied();
            let mut buf = tail.to_vec();
            encode_in_place(&mut buf, prev).unwrap();
            decode_in_place(&mut buf, prev).unwrap();
            prop_assert_eq!(buf.as_slice(), tail);
        }
    }

    #[test]
    fn gaps_follow_the_spec_formula(values in sorted_set(), prev in proptest::option::of(any::<u32>())) {
        let prev = prev.filter(|&p| values.first().is_none_or(|&v| p < v));
        let mut gaps = values.clone();
        encode_in_place(&mut gaps, prev).unwrap();
        let mut before = prev.map_or(-1_i64, i64::from);
        for (gap, value) in gaps.iter().zip(&values) {
            prop_assert_eq!(i64::from(*gap), i64::from(*value) - before - 1);
            before = i64::from(*value);
        }
    }

    #[test]
    fn decode_never_panics_and_is_increasing_or_overflow(gaps in proptest::collection::vec(any::<u32>(), 0..20), prev in proptest::option::of(any::<u32>())) {
        let mut buf = gaps.clone();
        let exact_last = prev.map_or(-1_i128, i128::from)
            + gaps.iter().map(|&g| i128::from(g) + 1).sum::<i128>();
        match decode_in_place(&mut buf, prev) {
            Ok(()) => {
                prop_assert!(exact_last <= i128::from(u32::MAX));
                prop_assert!(buf.windows(2).all(|w| w[0] < w[1]));
                prop_assert!(prev.is_none_or(|p| buf.first().is_none_or(|&f| p < f)));
            }
            Err(e) => {
                prop_assert_eq!(e, Corruption::ValueOverflow { bits: 32 });
                prop_assert!(exact_last > i128::from(u32::MAX));
            }
        }
    }

    // A local filter, not prop_assume!: about half of `sorted_set()` is shorter than 2, which exhausted the
    // global reject budget at PROPTEST_CASES=5000.
    #[test]
    fn non_increasing_input_is_rejected_at_its_index(values in sorted_set().prop_filter("two or more values", |v| v.len() >= 2), at in any::<prop::sample::Index>()) {
        let i = at.index(values.len() - 1) + 1;
        let mut bad = values.clone();
        bad[i] = bad[i - 1]; // a duplicate
        let mut buf = bad.clone();
        prop_assert_eq!(encode_in_place(&mut buf, None), Err(EncodeError::NotStrictlyIncreasing { index: i }));
        prop_assert_eq!(buf, bad);
    }
}

proptest! {
    /// SPEC §5.2.1: the encoder rejects the first value not above its predecessor (or `prev`), and a
    /// rejected buffer is left exactly as it was; accepted input round-trips.
    #[test]
    fn encode_rejects_at_the_first_violation_and_leaves_input_untouched(
        values in prop_oneof![
            proptest::collection::vec(prop_oneof![any::<u32>(), 0_u32..20, Just(u32::MAX)], 0..40),
            sorted_set(),
        ],
        prev in proptest::option::of(prop_oneof![any::<u32>(), 0_u32..20, Just(u32::MAX)]),
    ) {
        let mut last = prev;
        let first_bad = values.iter().position(|&v| {
            let bad = last.is_some_and(|l| v <= l);
            last = Some(v);
            bad
        });
        let mut buf = values.clone();
        match (encode_in_place(&mut buf, prev), first_bad) {
            (Ok(()), None) => {
                decode_in_place(&mut buf, prev).unwrap();
                prop_assert_eq!(buf, values);
            }
            (Err(e), Some(index)) => {
                prop_assert_eq!(e, EncodeError::NotStrictlyIncreasing { index });
                prop_assert_eq!(buf, values);
            }
            (result, expected) => prop_assert!(false, "got {result:?}, first violation at {expected:?}"),
        }
    }
}

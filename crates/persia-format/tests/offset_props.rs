//! Property tests for `Offset`/`Len` (ROADMAP 1.1.4): checked ops agree with exact `u128` arithmetic.

use persia_format::{Len, Offset};
use proptest::prelude::*;

/// Values near the edges, where overflow lives, plus uniform ones.
fn edgy() -> impl Strategy<Value = u64> {
    prop_oneof![
        any::<u64>(),
        (0_u64..64),
        (0_u64..64).prop_map(|d| u64::MAX - d)
    ]
}

proptest! {
    #[test]
    fn checked_ops_match_u128(a in edgy(), b in edgy(), k in edgy()) {
        let exact = |n: u128| u64::try_from(n).ok();
        let (wa, wb) = (u128::from(a), u128::from(b));
        prop_assert_eq!(Offset::new(a).checked_add(Len::new(b)).map(Offset::get), exact(wa + wb));
        prop_assert_eq!(Len::new(a).checked_add(Len::new(b)).map(Len::get), exact(wa + wb));
        prop_assert_eq!(Len::new(a).checked_mul(k).map(Len::get), exact(wa * u128::from(k)));
        prop_assert_eq!(Offset::new(a).checked_since(Offset::new(b)).map(Len::get), a.checked_sub(b));
        prop_assert_eq!(Len::new(a).checked_sub(Len::new(b)).map(Len::get), a.checked_sub(b));
    }

    #[test]
    fn span_is_some_iff_inside_the_limit(off in edgy(), len in edgy(), limit in edgy()) {
        let inside = u128::from(off) + u128::from(len) <= u128::from(limit);
        let span = Offset::new(off).span(Len::new(len), Len::new(limit));
        prop_assert_eq!(span.is_some(), inside);
        if let Some(range) = span {
            prop_assert_eq!(range.start as u64, off);
            prop_assert_eq!(range.len() as u64, len);
        }
    }
}

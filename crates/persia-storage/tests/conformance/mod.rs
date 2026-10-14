//! The `Storage` conformance suite (ROADMAP 2.2.3): the contract of SPEC §10.1, checked against any backend.
//!
//! A backend's test crate declares `mod conformance;` and invokes [`storage_conformance!`] with an expression that
//! builds a fresh, empty store; every check becomes its own `#[test]`. Checks that depend on a capability read
//! [`Storage::capabilities`] and test the documented fallback (e.g. `Unsupported`) when it is absent.
//!
//! Teardown is the store's job: the expression runs once per test and once per model case, so a local backend owns
//! its temporary directory (dropped with it), and a cloud backend works under a unique prefix per store and deletes
//! it on drop (or the emulator is discarded after the run).

// Test-only code: unwraps and indexing are the point.
#![allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Barrier;

use bytes::Bytes;
use persia_storage::{Error, ErrorKind, Key, KeyPrefix, PutOutcome, Storage};
use proptest::prelude::*;

/// Model cases by default: enough for a fast in-process backend; slow and remote backends pass fewer.
pub const MODEL_CASES: u32 = if cfg!(feature = "slow") { 2048 } else { 128 };

/// Emits one `#[test]` per conformance check; `$make` is evaluated once per test (and per model case) and must
/// yield a fresh, empty backend. `cases = n` sets the number of random operation sequences checked against the
/// model (default [`MODEL_CASES`]); failing sequences are saved next to the test file, like any proptest.
#[macro_export]
macro_rules! storage_conformance {
    ($make:expr) => {
        $crate::storage_conformance!($make, cases = $crate::conformance::MODEL_CASES);
    };
    ($make:expr, cases = $cases:expr) => {
        $crate::storage_conformance!(@tests $make;
            missing_objects_are_not_found,
            put_then_get_round_trips,
            put_replaces_the_object,
            get_range_returns_exactly_the_range_at_its_offset,
            get_range_past_the_end_is_a_short_read,
            inverted_ranges_are_invalid_input,
            longest_keys_round_trip,
            put_if_absent_creates_once,
            put_if_absent_finding_equal_bytes_is_created,
            list_is_sorted_and_filtered_by_byte_prefix,
            delete_is_idempotent,
            concurrent_put_if_absent_has_one_winner,
            concurrent_readers_never_see_a_torn_put,
        );
        proptest::proptest! {
            #![proptest_config(proptest::prelude::ProptestConfig::with_cases($cases))]
            #[test]
            fn operations_match_a_model(ops in $crate::conformance::model_ops()) {
                $crate::conformance::check_against_model(&$make, &ops)?;
            }
        }
    };
    (@tests $make:expr; $($check:ident,)*) => {
        $(
            #[test]
            fn $check() {
                $crate::conformance::$check(&|| $make);
            }
        )*
    };
}

/// The constructor every check receives.
pub type Make<'a, S> = &'a dyn Fn() -> S;

pub fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

pub fn prefix(s: &str) -> KeyPrefix {
    KeyPrefix::new(s).unwrap()
}

fn kind<T: std::fmt::Debug>(r: persia_storage::Result<T>) -> ErrorKind {
    r.unwrap_err().kind()
}

fn listed(s: &dyn Storage, p: &str) -> Vec<(String, u64)> {
    s.list(&prefix(p))
        .unwrap()
        .into_iter()
        .map(|m| (m.key.as_str().to_owned(), m.size))
        .collect()
}

pub fn missing_objects_are_not_found<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let k = key("segments/c/1.seg");
    assert_eq!(kind(s.get(&k)), ErrorKind::NotFound);
    assert_eq!(kind(s.get_range(&k, 0..0)), ErrorKind::NotFound);
    assert_eq!(kind(s.get_range(&k, 0..4)), ErrorKind::NotFound);
}

pub fn put_then_get_round_trips<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let data = Bytes::from((0..=255u8).cycle().take(70_000).collect::<Vec<_>>());
    s.put(&key("a/big"), data.clone()).unwrap();
    s.put(&key("a/empty"), Bytes::new()).unwrap();
    assert_eq!(s.get(&key("a/big")).unwrap(), data);
    assert_eq!(s.get(&key("a/empty")).unwrap(), Bytes::new());
    assert!(s.get_range(&key("a/empty"), 0..0).unwrap().is_empty());
}

pub fn put_replaces_the_object<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let k = key("k");
    s.put(&k, Bytes::from_static(b"a longer first version"))
        .unwrap();
    s.put(&k, Bytes::from_static(b"short")).unwrap();
    assert_eq!(&s.get(&k).unwrap()[..], b"short");
    if s.capabilities().strong_list {
        assert_eq!(listed(&s, ""), [("k".to_owned(), 5)]);
    }
}

pub fn get_range_returns_exactly_the_range_at_its_offset<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let k = key("k");
    s.put(&k, Bytes::from_static(b"0123456789")).unwrap();
    for range in [0..10, 0..1, 3..7, 9..10, 4..4, 10..10, 0..0] {
        let src = s.get_range(&k, range.clone()).unwrap();
        assert_eq!(
            (src.base_offset(), src.end_offset()),
            (range.start, range.end)
        );
        let (start, end) = (
            usize::try_from(range.start).unwrap(),
            usize::try_from(range.end).unwrap(),
        );
        assert_eq!(&*src, &b"0123456789"[start..end], "{range:?}");
    }
}

pub fn get_range_past_the_end_is_a_short_read<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let k = key("k");
    s.put(&k, Bytes::from_static(b"0123456789")).unwrap();
    for range in [8..11, 0..11, 10..11, 11..11, 20..30, 0..u64::MAX] {
        let err = s.get_range(&k, range.clone()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Corrupt, "{range:?}: {err}");
        let available = 10u64.saturating_sub(range.start);
        assert!(
            matches!(&err, Error::ShortRead { requested, returned, .. }
                if *requested == range && *returned == available),
            "{range:?}: {err}"
        );
    }
}

#[allow(clippy::reversed_empty_ranges)] // inverted ranges are the input under test
pub fn inverted_ranges_are_invalid_input<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let k = key("k");
    // Checked before any I/O: the object need not exist.
    assert_eq!(kind(s.get_range(&k, 5..4)), ErrorKind::InvalidInput);
    s.put(&k, Bytes::from_static(b"0123456789")).unwrap();
    assert_eq!(kind(s.get_range(&k, 5..4)), ErrorKind::InvalidInput);
    assert_eq!(kind(s.get_range(&k, u64::MAX..0)), ErrorKind::InvalidInput);
}

/// The longest key (1024 bytes, SPEC §10.1) with the longest segments the local backend stores (SPEC §10.1.1): 255
/// bytes as the last segment, 254 before a `/` (its directory name adds a marker byte).
pub fn longest_keys_round_trip<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let segs = [254, 254, 254, 3, 255].map(|n| "x".repeat(n));
    let k = key(&segs.join("/"));
    assert_eq!(k.as_str().len(), Key::MAX_LEN);
    s.put(&k, Bytes::from_static(b"long")).unwrap();
    assert_eq!(&s.get(&k).unwrap()[..], b"long");
    assert_eq!(&*s.get_range(&k, 1..3).unwrap(), b"on");
    if s.capabilities().strong_list {
        assert_eq!(
            listed(&s, &segs[..2].join("/")),
            [(k.as_str().to_owned(), 4)]
        );
    }
    s.delete(&k).unwrap();
    assert_eq!(kind(s.get(&k)), ErrorKind::NotFound);
}

pub fn put_if_absent_creates_once<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let k = key("manifests/00000001");
    if !s.capabilities().conditional_put {
        assert_eq!(
            kind(s.put_if_absent(&k, Bytes::from_static(b"m"))),
            ErrorKind::Unsupported
        );
        assert_eq!(kind(s.get(&k)), ErrorKind::NotFound, "nothing written");
        return;
    }
    let first = Bytes::from_static(b"manifest by writer A");
    assert_eq!(
        s.put_if_absent(&k, first.clone()).unwrap(),
        PutOutcome::Created
    );
    let other = Bytes::from_static(b"manifest by writer B");
    assert_eq!(
        s.put_if_absent(&k, other).unwrap(),
        PutOutcome::AlreadyExists
    );
    assert_eq!(
        s.get(&k).unwrap(),
        first,
        "the loser leaves the object unchanged"
    );
    // A plain put's object counts as existing too; after a delete the key is free again.
    let k2 = key("manifests/00000002");
    s.put(&k2, Bytes::from_static(b"x")).unwrap();
    assert_eq!(
        s.put_if_absent(&k2, Bytes::from_static(b"y")).unwrap(),
        PutOutcome::AlreadyExists
    );
    s.delete(&k).unwrap();
    assert_eq!(
        s.put_if_absent(&k, Bytes::from_static(b"again")).unwrap(),
        PutOutcome::Created
    );
}

pub fn put_if_absent_finding_equal_bytes_is_created<S: Storage>(make: Make<'_, S>) {
    let s = make();
    if !s.capabilities().conditional_put {
        return;
    }
    // SPEC §10.1: the object found may be this caller's own earlier attempt, so equal bytes resolve to Created.
    let k = key("manifests/00000001");
    let data = Bytes::from_static(b"manifest with fence token 7");
    assert_eq!(
        s.put_if_absent(&k, data.clone()).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        s.put_if_absent(&k, data.clone()).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(s.get(&k).unwrap(), data);
}

pub fn list_is_sorted_and_filtered_by_byte_prefix<S: Storage>(make: Make<'_, S>) {
    let s = make();
    // Inserted out of order; "b0" sorts after "b/…" ('/' < '0') and matches prefix "b" but not "b/". "é" (precomposed,
    // NFC) must be listed byte-exact: a filesystem that normalizes names (HFS+ stores NFD) would list another key.
    for (k, len) in [
        ("c", 3),
        ("b0", 2),
        ("b/2", 0),
        ("a", 1),
        ("b/1/x", 4),
        ("é", 1),
    ] {
        s.put(&key(k), Bytes::from(vec![7; len])).unwrap();
    }
    if !s.capabilities().strong_list {
        // A just-written object may be missing, but what is listed must still be sorted, filtered and sized.
        let all = listed(&s, "");
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0), "{all:?}");
        return;
    }
    let own = |v: &[(&str, u64)]| -> Vec<(String, u64)> {
        v.iter().map(|(k, n)| ((*k).to_owned(), *n)).collect()
    };
    assert_eq!(
        listed(&s, ""),
        own(&[
            ("a", 1),
            ("b/1/x", 4),
            ("b/2", 0),
            ("b0", 2),
            ("c", 3),
            ("é", 1)
        ])
    );
    assert_eq!(listed(&s, "b"), own(&[("b/1/x", 4), ("b/2", 0), ("b0", 2)]));
    assert_eq!(listed(&s, "b/"), own(&[("b/1/x", 4), ("b/2", 0)]));
    assert_eq!(listed(&s, "b/1"), own(&[("b/1/x", 4)]));
    assert_eq!(listed(&s, "b/1/x"), own(&[("b/1/x", 4)]));
    assert_eq!(listed(&s, "d"), own(&[]));
    assert_eq!(
        listed(&s, "b/."),
        own(&[]),
        "a prefix is never resolved as a path"
    );
}

pub fn delete_is_idempotent<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let k = key("k");
    s.delete(&k).unwrap();
    s.put(&k, Bytes::from_static(b"x")).unwrap();
    s.put(&key("k2"), Bytes::from_static(b"y")).unwrap();
    s.delete(&k).unwrap();
    assert_eq!(kind(s.get(&k)), ErrorKind::NotFound);
    s.delete(&k).unwrap();
    assert_eq!(
        &s.get(&key("k2")).unwrap()[..],
        b"y",
        "only the key is deleted"
    );
    if s.capabilities().strong_list {
        assert_eq!(listed(&s, ""), [("k2".to_owned(), 1)]);
    }
}

pub fn concurrent_put_if_absent_has_one_winner<S: Storage>(make: Make<'_, S>) {
    const WRITERS: usize = 8;
    let s = make();
    if !s.capabilities().conditional_put {
        return;
    }
    for round in 0..20 {
        let k = key(&format!("manifests/{round:08}"));
        let barrier = Barrier::new(WRITERS);
        let attempts: Vec<(Bytes, persia_storage::Result<PutOutcome>)> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..WRITERS)
                    .map(|w| {
                        let (s, k, barrier) = (&s, &k, &barrier);
                        scope.spawn(move || {
                            let data = Bytes::from(format!("manifest by writer {w}"));
                            barrier.wait();
                            (data.clone(), s.put_if_absent(k, data))
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
        // SPEC §10.1: an error (retries exhausted, e.g. a storm of conditional-write conflicts) leaves the outcome
        // unknown, so only a winner that answered is checked against the stored bytes.
        let winners: Vec<&Bytes> = attempts
            .iter()
            .filter(|(_, o)| matches!(o, Ok(PutOutcome::Created)))
            .map(|(d, _)| d)
            .collect();
        assert!(winners.len() <= 1, "round {round}: {attempts:?}");
        let stored = s.get(&k).unwrap();
        match winners.first() {
            Some(winner) => assert_eq!(&stored, *winner, "round {round}"),
            None => assert!(
                attempts.iter().any(|(d, o)| o.is_err() && *d == stored),
                "round {round}: no winner, stored {stored:?}: {attempts:?}"
            ),
        }
    }
}

pub fn concurrent_readers_never_see_a_torn_put<S: Storage>(make: Make<'_, S>) {
    let s = make();
    let k = key("k");
    let versions = [Bytes::from(vec![1u8; 4096]), Bytes::from(vec![2u8; 100])];
    s.put(&k, versions[0].clone()).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..500 {
                s.put(&k, versions[i % 2].clone()).unwrap();
            }
        });
        scope.spawn(|| {
            for _ in 0..500 {
                let got = s.get(&k).unwrap();
                assert!(versions.contains(&got), "torn read of {} bytes", got.len());
            }
        });
        // Ranged reads are a separate code path in most backends; both versions cover 0..100.
        scope.spawn(|| {
            for _ in 0..500 {
                let got = s.get_range(&k, 0..100).unwrap();
                assert!(
                    versions.iter().any(|v| *got == v[..100]),
                    "torn ranged read"
                );
            }
        });
    });
}

/// One step of [`operations_match_a_model`].
#[derive(Debug, Clone)]
pub enum ModelOp {
    Put(Key, Bytes),
    PutIfAbsent(Key, Bytes),
    Delete(Key),
    Get(Key),
    GetRange(Key, u64, u64),
    List(KeyPrefix),
}

/// Keys from a tiny alphabet, so operations collide and prefixes overlap at byte (not segment) boundaries.
fn model_key() -> impl Strategy<Value = Key> {
    let seg = prop_oneof![Just("a"), Just("b"), Just("a0")];
    prop::collection::vec(seg, 1..=2).prop_map(|segs| key(&segs.join("/")))
}

fn model_op() -> impl Strategy<Value = ModelOp> {
    let data = prop::collection::vec(any::<u8>(), 0..8).prop_map(Bytes::from);
    let pfx =
        prop_oneof![Just(""), Just("a"), Just("a/"), Just("a0"), Just("b/a")].prop_map(prefix);
    prop_oneof![
        3 => (model_key(), data.clone()).prop_map(|(k, d)| ModelOp::Put(k, d)),
        2 => (model_key(), data).prop_map(|(k, d)| ModelOp::PutIfAbsent(k, d)),
        2 => model_key().prop_map(ModelOp::Delete),
        2 => model_key().prop_map(ModelOp::Get),
        2 => (model_key(), 0..10u64, 0..10u64).prop_map(|(k, a, b)| ModelOp::GetRange(k, a, b)),
        1 => pfx.prop_map(ModelOp::List),
    ]
}

/// Applies `ops` to the backend and to a `BTreeMap`, comparing every answer (SPEC §10.1).
pub fn check_against_model(s: &dyn Storage, ops: &[ModelOp]) -> Result<(), TestCaseError> {
    let caps = s.capabilities();
    let mut model: BTreeMap<Key, Bytes> = BTreeMap::new();
    for op in ops {
        match op {
            ModelOp::Put(k, d) => {
                s.put(k, d.clone()).unwrap();
                model.insert(k.clone(), d.clone());
            }
            ModelOp::PutIfAbsent(k, d) => {
                let got = s.put_if_absent(k, d.clone());
                if !caps.conditional_put {
                    prop_assert_eq!(got.unwrap_err().kind(), ErrorKind::Unsupported);
                    continue;
                }
                let expected = match model.get(k) {
                    Some(existing) if existing != d => PutOutcome::AlreadyExists,
                    _ => PutOutcome::Created,
                };
                model.entry(k.clone()).or_insert_with(|| d.clone());
                prop_assert_eq!(got.unwrap(), expected, "{:?}", op);
            }
            ModelOp::Delete(k) => {
                s.delete(k).unwrap();
                model.remove(k);
            }
            ModelOp::Get(k) => {
                let got = s.get(k).map_err(|err| err.kind());
                prop_assert_eq!(got, model.get(k).cloned().ok_or(ErrorKind::NotFound));
            }
            ModelOp::GetRange(k, start, end) => {
                let got = s.get_range(k, *start..*end);
                let expected = match model.get(k) {
                    _ if start > end => Err(ErrorKind::InvalidInput),
                    None => Err(ErrorKind::NotFound),
                    Some(d) if *end > u64::try_from(d.len()).unwrap() => Err(ErrorKind::Corrupt),
                    Some(d) => Ok(
                        d.slice(usize::try_from(*start).unwrap()..usize::try_from(*end).unwrap())
                    ),
                };
                match (got, expected) {
                    (Ok(src), Ok(want)) => {
                        prop_assert_eq!(src.base_offset(), *start);
                        prop_assert_eq!(&*src, &want[..]);
                    }
                    (Err(err), Err(want)) => prop_assert_eq!(err.kind(), want, "{:?}", op),
                    (got, want) => prop_assert!(false, "{:?}: got {:?}, want {:?}", op, got, want),
                }
            }
            ModelOp::List(p) => {
                if !caps.strong_list {
                    continue;
                }
                let got: Vec<(Key, u64)> = s
                    .list(p)
                    .unwrap()
                    .into_iter()
                    .map(|m| (m.key, m.size))
                    .collect();
                let want: Vec<(Key, u64)> = model
                    .iter()
                    .filter(|(k, _)| p.matches(k))
                    .map(|(k, d)| (k.clone(), u64::try_from(d.len()).unwrap()))
                    .collect();
                prop_assert_eq!(got, want, "{:?}", op);
            }
        }
    }
    Ok(())
}

/// Random operation sequences for [`check_against_model`].
pub fn model_ops() -> impl Strategy<Value = Vec<ModelOp>> {
    prop::collection::vec(model_op(), 0..40)
}

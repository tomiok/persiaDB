//! `put_blob`, `get_blob` and `get_blob_range` (SPEC §9.2, §9.5, §9.6; ROADMAP 9.4.2, ADR-0034, ADR-0051).
//!
//! Invariants:
//! 1. Any blob, at any chunk size a writer uses, reads back byte for byte, by range and streamed; its id is its
//!    SHA-256 and its reference names the chunk size it is stored with.
//! 2. An `expected_id` that differs binds nothing and discards the upload; one that matches stores the blob.
//! 3. A duplicate returns the bound blob's chunk size and discards its own upload, never the bound one's.
//! 4. A bind whose outcome is unknown is retried with the same manifest and its upload is never discarded.
//! 5. A store error while staging binds nothing; a chunk size below the floor or an invalid `mime` reads nothing.
//! 6. The upload id is pinned at every chunk write, and the blob id at the bind; both until the guard drops.
//! 7. A small blob (≤ 64 KiB) goes through `bind_small`; a larger one is staged in bounded windows.
//! 8. A `get_blob` stream pins its blob and upload until it is dropped, so a GC sweep never takes it mid-stream.
//! 9. A store that refuses every write until reopened (`Error::is_store_poisoned`) ends the bind retry at once, as an
//!    unknown outcome that discards nothing, instead of at the end of `bind_budget` (ROADMAP 9.4.7).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::assert_is_empty
)] // Tests: inputs are fixed.

#[path = "support/store.rs"]
mod store;

use std::io::Read;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use persia::internal::blob::{Blobs, PutBlobOptions, WINDOW_MAX_CHUNKS};
use persia::internal::pins::{PinSet, Pinned};
use persia::internal::retry::{FIRST_BACKOFF, MAX_BACKOFF, ManualRetryClock, RetryClock};
use persia_blob::{
    BlobId, BlobStore, ChunkSize, Error, MemoryBlobStore, SMALL_BLOB_MAX, UploadId, blob_id_of,
};
use persia_engine::db::SegmentIds;
use proptest::prelude::*;
use store::{Call, Faults, FaultyStore};

const MIN: u32 = ChunkSize::MIN;

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn options(chunk: u32) -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(chunk).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn faulty(faults: Faults) -> (Arc<FaultyStore>, Blobs<FaultyStore>) {
    let store = FaultyStore::new(faults);
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(11))
        .with_retry_clock(Arc::new(ManualRetryClock::new()));
    store.watch(blobs.pin_set());
    (store, blobs)
}

fn upload_of(pins: &persia::internal::pins::Pins) -> UploadId {
    match pins.ids().first() {
        Some(Pinned::Upload(u)) => *u,
        other => panic!("first pin is not the upload: {other:?}"),
    }
}

fn streamed(
    blobs: &Blobs<impl BlobStore + ?Sized>,
    r: &persia_blob::BlobRef,
    buf: usize,
) -> Vec<u8> {
    let mut reader = blobs.get_blob(r).unwrap();
    let mut out = Vec::new();
    let mut piece = vec![0; buf];
    loop {
        let n = reader.read(&mut piece).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&piece[..n]);
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 256 } else { 24 }, ..ProptestConfig::default() })]

    /// Invariants 1, 2, 7.
    #[test]
    fn any_blob_reads_back_by_range_and_streamed(
        len in prop_oneof![0..=16usize, 65_530..=65_540usize, 65_536..=300_000usize],
        seed in any::<u8>(),
        chunk in prop_oneof![Just(MIN), Just(MIN + 1), Just(100_000u32), Just(4 << 20)],
        expected in 0..3u8,
        ranges in proptest::collection::vec((any::<u64>(), any::<u64>()), 4),
        buf in 1..70_000usize,
    ) {
        let data = bytes(len, seed);
        let id = blob_id_of(&data);
        let (store, blobs) = faulty(Faults::default());
        let wrong = blob_id_of(b"not these bytes");
        let expected_id = match expected { 0 => None, 1 => Some(id), _ => Some(wrong) };
        let opts = PutBlobOptions { expected_id, ..options(chunk) };
        let result = blobs.put_blob(&data[..], &opts);
        if expected == 2 {
            let mismatch = matches!(result, Err(Error::BlobIdMismatch { expected, actual }) if expected == wrong && actual == id);
            prop_assert!(mismatch);
            prop_assert!(store.inner.bound_ids().is_empty());
            // A large blob staged chunks, which are discarded; a small one staged nothing to discard.
            let discarded = store.calls().iter().any(|c| matches!(c, Call::Discard(_)));
            prop_assert_eq!(discarded, len as u64 > SMALL_BLOB_MAX);
            prop_assert!(blobs.pin_set().pinned().is_empty());
            return Ok(());
        }
        let (r, pins) = result.unwrap();
        prop_assert_eq!(r.blob_id(), id);
        prop_assert_eq!(r.size(), len as u64);
        prop_assert_eq!(r.chunk_size(), chunk);
        prop_assert_eq!(r.mime(), "application/octet-stream");
        prop_assert!(store.contains(&r).unwrap());
        let small = len as u64 <= SMALL_BLOB_MAX;
        let binds: Vec<_> = store.calls().into_iter().filter_map(|c| match c {
            Call::Bind { small, .. } => Some(small),
            _ => None,
        }).collect();
        prop_assert_eq!(binds, vec![small]);
        prop_assert_eq!(blobs.get_blob_range(&r, 0..len as u64).unwrap(), data.clone());
        for (a, b) in ranges {
            let (a, b) = (a % (len as u64 + 1), b % (len as u64 + 1));
            let (a, b) = (a.min(b), a.max(b));
            prop_assert_eq!(blobs.get_blob_range(&r, a..b).unwrap(), &data[a as usize..b as usize]);
        }
        prop_assert_eq!(streamed(&blobs, &r, buf), data);
        drop(pins);
        prop_assert!(blobs.pin_set().pinned().is_empty());
    }

    /// Invariant 3: the same bytes at another chunk size are the bound blob.
    #[test]
    fn a_duplicate_returns_the_bound_chunk_size(
        len in 0..200_000usize,
        first in prop_oneof![Just(MIN), Just(70_000u32), Just(4 << 20)],
        second in prop_oneof![Just(MIN), Just(70_000u32), Just(4 << 20)],
    ) {
        let data = bytes(len, 3);
        let (store, blobs) = faulty(Faults::default());
        let (a, pa) = blobs.put_blob(&data[..], &options(first)).unwrap();
        let (b, pb) = blobs.put_blob(&data[..], &options(second)).unwrap();
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(b.chunk_size(), first);
        let (ua, ub) = (upload_of(&pa), upload_of(&pb));
        prop_assert_ne!(ua, ub);
        prop_assert!(store.calls().contains(&Call::Discard(ub)));
        prop_assert!(!store.calls().contains(&Call::Discard(ua)));
        prop_assert_eq!(store.inner.staged_chunk_count(ub), 0);
        // The duplicate pins the bound blob's upload too, whose chunks it reads.
        prop_assert_eq!(pb.ids(), &[Pinned::Upload(ub), Pinned::Blob(b.blob_id()), Pinned::Upload(ua)][..]);
        prop_assert_eq!(blobs.get_blob_range(&b, 0..len as u64).unwrap(), data);
    }
}

#[test]
fn every_range_of_a_three_chunk_blob_reads_its_bytes() {
    let data = bytes(2 * MIN as usize + 17, 9);
    let blobs = Blobs::new(Arc::new(MemoryBlobStore::new()));
    let (r, _pins) = blobs.put_blob(&data[..], &options(MIN)).unwrap();
    let len = data.len() as u64;
    let edges = [
        0,
        1,
        65_535,
        65_536,
        65_537,
        131_071,
        131_072,
        131_073,
        len - 1,
        len,
    ];
    for &a in &edges {
        for &b in &edges {
            if a <= b {
                let got = blobs.get_blob_range(&r, a..b).unwrap();
                assert_eq!(got, &data[a as usize..b as usize], "{a}..{b}");
            } else {
                assert!(matches!(
                    blobs.get_blob_range(&r, a..b),
                    Err(Error::RangeOutOfBounds { .. })
                ));
            }
        }
    }
    assert!(matches!(
        blobs.get_blob_range(&r, 0..len + 1),
        Err(Error::RangeOutOfBounds { .. })
    ));
}

/// Invariant 7: chunks are staged a bounded window at a time, and streamed back a window at a time.
#[test]
fn chunks_are_staged_and_read_in_bounded_windows() {
    let count = WINDOW_MAX_CHUNKS as usize * 2 + 3;
    let data = bytes(MIN as usize * count, 1);
    let (store, blobs) = faulty(Faults::default());
    let (r, _pins) = blobs.put_blob(&data[..], &options(MIN)).unwrap();
    let windows: Vec<usize> = store
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::StageMany { count, .. } => Some(count),
            _ => None,
        })
        .collect();
    let w = WINDOW_MAX_CHUNKS as usize;
    assert_eq!(windows, [w, w, 3]);
    assert_eq!(streamed(&blobs, &r, 1 << 20), data);
    let reads: Vec<_> = store
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::ReadChunks(_, range) => Some(range),
            _ => None,
        })
        .collect();
    let w = WINDOW_MAX_CHUNKS;
    assert_eq!(reads, [0..w, w..2 * w, 2 * w..2 * w + 3]);
}

/// Invariant 6.
#[test]
fn the_upload_is_pinned_at_every_write_and_the_blob_at_the_bind() {
    for len in [10, 200_000] {
        let (store, blobs) = faulty(Faults::default());
        let (r, pins) = blobs.put_blob(&bytes(len, 2)[..], &options(MIN)).unwrap();
        let upload = upload_of(&pins);
        let calls = store.calls();
        for call in &calls {
            match call {
                Call::Stage { pinned, .. } => assert!(pinned, "{call:?}"),
                Call::Bind { pinned, .. } => assert_eq!(*pinned, (true, true), "{call:?}"),
                _ => {}
            }
        }
        assert!(calls.iter().any(|c| matches!(c, Call::Bind { .. })));
        assert_eq!(
            pins.ids(),
            [Pinned::Upload(upload), Pinned::Blob(r.blob_id())]
        );
        let set = Arc::clone(blobs.pin_set());
        assert!(set.is_pinned(Pinned::Blob(r.blob_id())));
        drop(pins);
        assert!(set.pinned().is_empty());
    }
}

/// Invariant 4: retried until it answers, never discarded, the store's own landing recognized.
#[test]
fn an_unknown_bind_outcome_is_retried_and_never_discarded() {
    for lands in [false, true] {
        for len in [10, 200_000] {
            let faults = Faults {
                unknown_binds: 3,
                unknown_lands: lands,
                ..Faults::default()
            };
            let (store, blobs) = faulty(faults);
            let data = bytes(len, 4);
            let (r, pins) = blobs.put_blob(&data[..], &options(MIN)).unwrap();
            let upload = upload_of(&pins);
            let calls = store.calls();
            let binds: Vec<_> = calls
                .iter()
                .filter_map(|c| match c {
                    Call::Bind {
                        upload,
                        blob,
                        small,
                        ..
                    } => Some((*upload, *blob, *small)),
                    _ => None,
                })
                .collect();
            // The same manifest each time: same upload, same id, and a small blob always through `bind_small`.
            let small = len as u64 <= SMALL_BLOB_MAX;
            assert_eq!(binds, vec![(upload, r.blob_id(), small); 4]);
            assert!(
                !calls.iter().any(|c| matches!(c, Call::Discard(_))),
                "{calls:?}"
            );
            assert_eq!(blobs.get_blob_range(&r, 0..len as u64).unwrap(), data);
        }
    }
}

#[test]
fn an_unknown_outcome_before_a_duplicate_discards_nothing() {
    let data = bytes(100, 5);
    let (store, blobs) = faulty(Faults::default());
    let (first, _p) = blobs.put_blob(&data[..], &options(MIN)).unwrap();
    store.set_faults(Faults {
        unknown_binds: 1,
        ..Faults::default()
    });
    let (second, _q) = blobs.put_blob(&data[..], &options(MIN)).unwrap();
    assert_eq!(first, second);
    assert!(!store.calls().iter().any(|c| matches!(c, Call::Discard(_))));
}

/// The retry runs for its time budget, waiting a growing, jittered backoff between attempts, then fails with the
/// unknown outcome and discards nothing (SPEC §9.4, ADR-0051 item 3).
#[test]
fn a_bind_that_stays_unknown_for_its_budget_fails_and_discards_nothing() {
    for len in [10, 200_000] {
        let store = FaultyStore::new(Faults {
            unknown_binds: u32::MAX,
            ..Faults::default()
        });
        let clock = Arc::new(ManualRetryClock::new());
        let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(11))
            .with_retry_clock(Arc::clone(&clock) as _);
        let budget = Duration::from_secs(30);
        let opts = PutBlobOptions {
            bind_budget: budget,
            ..options(MIN)
        };
        let error = blobs.put_blob(&bytes(len, 6)[..], &opts).unwrap_err();
        assert!(error.is_bind_outcome_unknown());
        let calls = store.calls();
        let binds = calls
            .iter()
            .filter(|c| matches!(c, Call::Bind { .. }))
            .count();
        let sleeps = clock.sleeps();
        // One wait between two attempts, none longer than the cap.
        assert_eq!(binds, sleeps.len() + 1);
        assert!(sleeps.iter().all(|s| *s <= MAX_BACKOFF));
        // Every attempt starts before the deadline (attempt k starts after the first k waits); the wait that would
        // have reached it was not waited.
        assert!(starts(&sleeps).iter().all(|at| *at < budget), "{sleeps:?}");
        assert!(sleeps.iter().sum::<Duration>() > budget.checked_sub(MAX_BACKOFF).unwrap());
        // Equal jitter: wait n lies in [full/2, full], full = FIRST_BACKOFF·2^n capped at MAX_BACKOFF.
        for (n, wait) in sleeps.iter().enumerate() {
            let full = FIRST_BACKOFF
                .saturating_mul(1 << n.min(20))
                .min(MAX_BACKOFF);
            assert!(
                *wait >= full / 2 && *wait <= full,
                "wait {n}: {wait:?} outside {:?}..={full:?}",
                full / 2
            );
        }
        assert!(!calls.iter().any(|c| matches!(c, Call::Discard(_))));
        assert!(blobs.pin_set().pinned().is_empty());
    }
}

/// When attempt k starts, from the waits before it.
fn starts(sleeps: &[Duration]) -> Vec<Duration> {
    let mut at = Duration::ZERO;
    let mut out = vec![at];
    for s in sleeps {
        at += *s;
        out.push(at);
    }
    out
}

/// A clock whose waits last three times what was asked, like an overloaded machine.
#[derive(Debug, Default)]
struct Overshooting(ManualRetryClock);

impl RetryClock for Overshooting {
    fn now(&self) -> Duration {
        self.0.now()
    }

    fn sleep(&self, duration: Duration) {
        self.0.sleep(duration * 3);
    }
}

/// A wait that overruns the deadline is checked after it: no attempt starts at or after the deadline.
#[test]
fn no_attempt_starts_at_or_after_the_deadline() {
    for budget in [
        Duration::from_millis(1),
        Duration::from_millis(120),
        Duration::from_secs(7),
    ] {
        let store = FaultyStore::new(Faults {
            unknown_binds: u32::MAX,
            ..Faults::default()
        });
        let clock = Arc::new(Overshooting::default());
        let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(11))
            .with_retry_clock(Arc::clone(&clock) as _);
        let opts = PutBlobOptions {
            bind_budget: budget,
            ..options(MIN)
        };
        assert!(
            blobs
                .put_blob(&bytes(10, 6)[..], &opts)
                .unwrap_err()
                .is_bind_outcome_unknown()
        );
        let sleeps = clock.0.sleeps();
        let binds = store
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Bind { .. }))
            .count();
        let starts = starts(&sleeps);
        // Either the wait before the deadline was not waited (one attempt per start), or the last wait overran it and
        // the check after it caught that (no attempt followed it).
        assert!(
            binds == sleeps.len() + 1 || (binds == sleeps.len() && starts[binds] >= budget),
            "{budget:?}: {starts:?}"
        );
        assert!(
            starts[..binds].iter().all(|at| *at < budget),
            "{budget:?}: {starts:?}"
        );
    }
}

/// A zero budget still makes one attempt.
#[test]
fn a_zero_budget_binds_once() {
    let (store, blobs) = faulty(Faults::default());
    let opts = PutBlobOptions {
        bind_budget: Duration::ZERO,
        ..options(MIN)
    };
    let (_blob, _pins) = blobs.put_blob(&bytes(10, 1)[..], &opts).unwrap();
    let (store2, blobs2) = faulty(Faults {
        unknown_binds: u32::MAX,
        ..Faults::default()
    });
    assert!(
        blobs2
            .put_blob(&bytes(10, 1)[..], &opts)
            .unwrap_err()
            .is_bind_outcome_unknown()
    );
    for s in [store, store2] {
        let binds = s
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Bind { .. }))
            .count();
        assert_eq!(binds, 1);
    }
}

/// Invariant 5.
#[test]
fn a_stage_error_binds_nothing() {
    for fail in [0, 1, 2] {
        let (store, blobs) = faulty(Faults {
            fail_stage: Some(fail),
            ..Faults::default()
        });
        assert!(
            blobs
                .put_blob(&bytes(3 * MIN as usize, 7)[..], &options(MIN))
                .is_err()
        );
        let calls = store.calls();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Bind { .. })),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|c| matches!(c, Call::Discard(_))));
        assert!(store.inner.bound_ids().is_empty());
        assert!(blobs.pin_set().pinned().is_empty());
    }
}

#[test]
fn a_reader_error_binds_nothing() {
    struct Failing(usize);
    impl Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("reader failed"));
            }
            let n = buf.len().min(self.0);
            self.0 -= n;
            buf[..n].fill(1);
            Ok(n)
        }
    }
    for len in [0, 10, 200_000] {
        let (store, blobs) = faulty(Faults::default());
        assert!(matches!(
            blobs.put_blob(Failing(len), &options(MIN)),
            Err(Error::Read(_))
        ));
        assert!(store.inner.bound_ids().is_empty());
    }
}

#[test]
fn a_chunk_size_below_the_floor_or_a_bad_mime_reads_nothing() {
    struct Untouchable;
    impl Read for Untouchable {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("put_blob read a blob it should have rejected");
        }
    }
    let (store, blobs) = faulty(Faults::default());
    for chunk in [1, 4, MIN - 1] {
        let opts = PutBlobOptions {
            chunk_size: ChunkSize::from_encoding(chunk).unwrap(),
            ..PutBlobOptions::default()
        };
        assert!(matches!(
            blobs.put_blob(Untouchable, &opts),
            Err(Error::ChunkSizeBelowMin { chunk_size, min: MIN }) if chunk_size == chunk
        ));
    }
    for mime in ["", "caf\u{e9}/x"] {
        let opts = PutBlobOptions {
            mime: mime.to_owned(),
            ..PutBlobOptions::default()
        };
        assert!(matches!(
            blobs.put_blob(Untouchable, &opts),
            Err(Error::InvalidBlobRef(_))
        ));
    }
    let opts = PutBlobOptions {
        user_meta: [(String::new(), "v".to_owned())].into(),
        ..PutBlobOptions::default()
    };
    assert!(matches!(
        blobs.put_blob(Untouchable, &opts),
        Err(Error::InvalidBlobRef(_))
    ));
    assert!(store.calls().is_empty());
}

#[test]
fn mime_and_user_meta_are_carried_by_the_reference() {
    let blobs = Blobs::new(Arc::new(MemoryBlobStore::new()));
    let opts = PutBlobOptions {
        mime: "image/png".to_owned(),
        user_meta: [("filename".to_owned(), "a.png".to_owned())].into(),
        ..PutBlobOptions::default()
    };
    let (r, _pins) = blobs.put_blob(&b"png"[..], &opts).unwrap();
    assert_eq!(r.mime(), "image/png");
    assert_eq!(
        r.user_meta().get("filename").map(String::as_str),
        Some("a.png")
    );
    assert_eq!(r.chunk_size(), 4 << 20);
}

#[test]
fn a_reference_the_store_lacks_is_not_found() {
    let blobs = Blobs::new(Arc::new(MemoryBlobStore::new()));
    let (r, _pins) = blobs.put_blob(&b"x"[..], &options(MIN)).unwrap();
    let other = Blobs::new(Arc::new(MemoryBlobStore::new()));
    assert!(matches!(
        other.get_blob(&r),
        Err(Error::BlobNotFound { .. })
    ));
    assert!(matches!(
        other.get_blob_range(&r, 0..1),
        Err(Error::BlobNotFound { .. })
    ));
    assert!(other.pin_contained(&r).unwrap().is_none());
    assert!(other.pin_set().pinned().is_empty());
    let pins = blobs.pin_contained(&r).unwrap().unwrap();
    let upload = upload_of(&blobs.put_blob(&b"x"[..], &options(MIN)).unwrap().1);
    let bound = blobs
        .store()
        .manifest(r.blob_id())
        .unwrap()
        .unwrap()
        .upload_id();
    assert_ne!(upload, bound);
    // The blob, and the upload its manifest names.
    assert_eq!(
        pins.ids(),
        [Pinned::Blob(r.blob_id()), Pinned::Upload(bound)]
    );
}

#[test]
fn pins_of_one_id_are_counted() {
    let set = Arc::new(PinSet::new());
    let id = Pinned::Blob(BlobId::from_bytes([1; 32]));
    let a = set.pin([id]);
    let b = set.pin([id]);
    drop(a);
    assert!(set.is_pinned(id));
    assert_eq!(set.unless_pinned(id, |_| ()), None);
    drop(b);
    assert!(!set.is_pinned(id));
    assert_eq!(set.unless_pinned(id, |_| 1), Some(1));
}

/// The sweep's check and delete are one step: a pin asked for while the delete runs waits for it.
#[test]
fn a_pin_waits_for_a_delete_under_the_lock() {
    let set = Arc::new(PinSet::new());
    let id = Pinned::Upload(UploadId::new(3));
    let (tx, rx) = mpsc::channel();
    let pinner = Arc::clone(&set);
    let ran = set.unless_pinned(id, |_| {
        let thread = std::thread::spawn(move || {
            let pins = pinner.pin([id]);
            tx.send(()).unwrap();
            pins
        });
        // The pinning thread is blocked on the lock this delete holds.
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        thread
    });
    let pins = ran.unwrap().join().unwrap();
    assert!(set.is_pinned(id));
    drop(pins);
}

/// The sweep's delete reads the pins through the view it is given, without locking the set again: the check that no
/// pinned blob's manifest names an upload, run inside `delete`, finishes.
#[test]
fn a_delete_reads_the_pins_through_its_view() {
    let set = Arc::new(PinSet::new());
    let blob = Pinned::Blob(BlobId::from_bytes([2; 32]));
    let upload = Pinned::Upload(UploadId::new(4));
    let _pins = set.pin([blob]);
    let seen = set.unless_pinned(upload, |view| {
        (view.is_pinned(blob), view.is_pinned(upload), view.pinned())
    });
    assert_eq!(seen, Some((true, false, vec![blob])));
}

// --- Boundaries of put_blob (ROADMAP 9.4.2; SPEC §9.2: small blobs up to 64 KiB, fixed-size chunks) ---------------

/// The `StageMany` sizes, the bind kind, and the stored manifest of one `put_blob` of `len` bytes at `chunk`.
struct Stored {
    windows: Vec<usize>,
    small: bool,
    chunk_lens: Vec<usize>,
    crcs: usize,
}

fn store_and_inspect(data: &[u8], chunk: u32, reader: impl Read) -> Stored {
    let (store, blobs) = faulty(Faults::default());
    let (r, pins) = blobs.put_blob(reader, &options(chunk)).unwrap();
    assert_eq!(r.blob_id(), blob_id_of(data), "len {}", data.len());
    assert_eq!(r.size(), data.len() as u64);
    let calls = store.calls();
    let windows = calls
        .iter()
        .filter_map(|c| match c {
            Call::StageMany { count, .. } => Some(*count),
            _ => None,
        })
        .collect();
    let binds: Vec<bool> = calls
        .iter()
        .filter_map(|c| match c {
            Call::Bind { small, .. } => Some(*small),
            _ => None,
        })
        .collect();
    assert_eq!(binds.len(), 1, "len {}: one bind", data.len());
    let manifest = store.inner.manifest(r.blob_id()).unwrap().unwrap();
    let chunk_lens = (0..manifest.chunk_count())
        .map(|i| store.inner.read_chunk(&manifest, i).unwrap().len())
        .collect();
    assert_eq!(
        blobs.get_blob_range(&r, 0..data.len() as u64).unwrap(),
        data
    );
    assert_eq!(streamed(&blobs, &r, 4096), data);
    drop(pins);
    assert!(blobs.pin_set().pinned().is_empty());
    Stored {
        windows,
        small: binds[0],
        chunk_lens,
        crcs: manifest.chunk_crcs().len(),
    }
}

/// The boundaries of the small path (exactly 64 KiB is small, one byte more is staged) and of chunking (an exact
/// multiple of the chunk size has no empty last chunk, an exact multiple of the window stages no empty window).
#[test]
fn put_blob_boundaries_take_the_right_path_and_chunk_exactly() {
    let min = MIN as usize;
    let w = WINDOW_MAX_CHUNKS as usize;
    // (len, chunk size, small, chunk lengths as (count, len) runs, staged windows)
    #[allow(clippy::type_complexity)]
    let cases: &[(usize, u32, bool, &[(usize, usize)], &[usize])] = &[
        (0, MIN, true, &[], &[]),
        (1, MIN, true, &[(1, 1)], &[]),
        (min - 1, MIN, true, &[(1, min - 1)], &[]),
        (min, MIN, true, &[(1, min)], &[]),
        (min, 4 << 20, true, &[(1, min)], &[]),
        (min + 1, MIN, false, &[(1, min), (1, 1)], &[2]),
        (min + 1, 4 << 20, false, &[(1, min + 1)], &[1]),
        (2 * min, MIN, false, &[(2, min)], &[2]),
        (3 * min - 1, MIN, false, &[(2, min), (1, min - 1)], &[3]),
        (3 * min, MIN, false, &[(3, min)], &[3]),
        (3 * min + 1, MIN, false, &[(3, min), (1, 1)], &[4]),
        (2 * 70_000, 70_000, false, &[(2, 70_000)], &[2]),
        (w * min, MIN, false, &[(w, min)], &[w]),
        (w * min + 1, MIN, false, &[(w, min), (1, 1)], &[w, 1]),
        (2 * w * min, MIN, false, &[(2 * w, min)], &[w, w]),
    ];
    for &(len, chunk, small, runs, windows) in cases {
        let data = bytes(len, 5);
        let got = store_and_inspect(&data, chunk, &data[..]);
        let lens: Vec<usize> = runs
            .iter()
            .flat_map(|&(n, l)| std::iter::repeat_n(l, n))
            .collect();
        assert_eq!(got.small, small, "len {len}, chunk {chunk}: small path");
        assert_eq!(got.chunk_lens, lens, "len {len}, chunk {chunk}: chunks");
        assert_eq!(
            got.crcs,
            lens.len(),
            "len {len}, chunk {chunk}: one CRC per chunk"
        );
        assert_eq!(got.windows, windows, "len {len}, chunk {chunk}: windows");
    }
}

/// The empty blob: small, no chunk, its id the SHA-256 of nothing; it reads back empty, and any non-empty range is
/// out of bounds.
#[test]
fn the_empty_blob_is_stored_and_reads_back_empty() {
    let blobs = Blobs::new(Arc::new(MemoryBlobStore::new()));
    let (r, _pins) = blobs.put_blob(std::io::empty(), &options(MIN)).unwrap();
    assert_eq!(r.blob_id(), blob_id_of(&[]));
    assert_eq!(r.size(), 0);
    assert!(blobs.store().contains(&r).unwrap());
    assert_eq!(blobs.get_blob_range(&r, 0..0).unwrap(), Vec::<u8>::new());
    assert!(matches!(
        blobs.get_blob_range(&r, 0..1),
        Err(Error::RangeOutOfBounds { .. })
    ));
    let mut out = Vec::new();
    blobs.get_blob(&r).unwrap().read_to_end(&mut out).unwrap();
    assert!(out.is_empty());
    // A second empty blob is the same blob.
    let (again, _p) = blobs.put_blob(&b""[..], &options(4 << 20)).unwrap();
    assert_eq!(again, r);
}

/// A reader that hands out its bytes in the given piece lengths, cycled; 0 is an `Interrupted` error (which a reader
/// may return at any call, `std::io::Read`), like a socket or a pipe.
struct Trickle {
    data: Vec<u8>,
    at: usize,
    pieces: &'static [usize],
    call: usize,
}

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let piece = self.pieces[self.call % self.pieces.len()];
        self.call += 1;
        if piece == 0 {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let n = piece.min(buf.len()).min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Short reads and `Interrupted` errors, also at the first call and around the 64 KiB boundary, store the same blob
/// with the same chunks as one whole read.
#[test]
fn short_reads_and_interruptions_store_the_same_blob() {
    let min = MIN as usize;
    let patterns: [&'static [usize]; 4] = [
        &[1],
        &[0, 7, 0, 0, 4096, 1],
        &[65_535, 0, 2, 65_539],
        &[0, 100_000, 3, 0, 65_535],
    ];
    for len in [0, 1, min - 1, min, min + 1, 3 * min, 3 * min + 5] {
        let data = bytes(len, 7);
        let whole = store_and_inspect(&data, MIN, &data[..]);
        for pieces in patterns {
            if pieces == [1] && len > 3 * min {
                continue; // A byte at a time is slow in debug builds; the other lengths cover it.
            }
            let trickle = Trickle {
                data: data.clone(),
                at: 0,
                pieces,
                call: 0,
            };
            let got = store_and_inspect(&data, MIN, trickle);
            let ctx = format!("len {len}, pieces {pieces:?}");
            assert_eq!(got.small, whole.small, "{ctx}");
            assert_eq!(got.chunk_lens, whole.chunk_lens, "{ctx}");
            assert_eq!(got.windows, whole.windows, "{ctx}");
        }
    }
}

/// An `expected_id` mismatch while the bytes' blob (or the expected one) is already bound binds nothing, discards
/// only the new upload, and leaves the bound blob, its upload's chunks and its pins as they were.
#[test]
fn an_expected_id_mismatch_with_a_duplicate_present_leaves_the_bound_blob() {
    for len in [10, 200_000] {
        let data = bytes(len, 1);
        let other = bytes(len, 2);
        let (store, blobs) = faulty(Faults::default());
        let (bound, pins) = blobs.put_blob(&data[..], &options(MIN)).unwrap();
        let bound_upload = upload_of(&pins);
        let bound_chunks = store.inner.staged_chunk_count(bound_upload);
        let pinned = blobs.pin_set().pinned();
        // The same bytes expected to be others, and other bytes expected to be the bound ones.
        for (bytes, expected) in [(&data, blob_id_of(&other)), (&other, bound.blob_id())] {
            let ctx = format!("len {len}, expected {expected}");
            let before = store.calls().len();
            let opts = PutBlobOptions {
                expected_id: Some(expected),
                ..options(MIN)
            };
            let Err(Error::BlobIdMismatch {
                expected: e,
                actual,
            }) = blobs.put_blob(&bytes[..], &opts)
            else {
                panic!("{ctx}: no mismatch");
            };
            assert_eq!((e, actual), (expected, blob_id_of(bytes)), "{ctx}");
            let calls = &store.calls()[before..];
            assert!(
                !calls.iter().any(|c| matches!(c, Call::Bind { .. })),
                "{ctx}: bound"
            );
            let discarded: Vec<_> = calls
                .iter()
                .filter_map(|c| match c {
                    Call::Discard(u) => Some(*u),
                    _ => None,
                })
                .collect();
            // A small blob staged nothing, so it discards nothing; a large one discards its own upload.
            let staged = bytes.len() as u64 > SMALL_BLOB_MAX;
            assert_eq!(discarded.len(), usize::from(staged), "{ctx}: {calls:?}");
            assert!(
                !discarded.contains(&bound_upload),
                "{ctx}: discarded the bound upload"
            );
            for u in &discarded {
                assert_eq!(store.inner.staged_chunk_count(*u), 0, "{ctx}");
            }
            assert_eq!(store.inner.bound_ids(), [bound.blob_id()], "{ctx}");
            assert_eq!(
                store.inner.staged_chunk_count(bound_upload),
                bound_chunks,
                "{ctx}"
            );
            assert_eq!(blobs.pin_set().pinned(), pinned, "{ctx}: pins changed");
            assert_eq!(
                blobs.get_blob_range(&bound, 0..len as u64).unwrap(),
                data,
                "{ctx}"
            );
        }
        drop(pins);
        assert!(blobs.pin_set().pinned().is_empty());
    }
}

/// An unknown bind outcome, then plain store errors on every retry: a plain error after an unknown outcome is no
/// answer (the first write may still land), so the retry goes on for its whole budget, then fails with the unknown
/// outcome and discards nothing (SPEC §9.4, ADR-0051 item 3).
#[test]
fn an_unknown_outcome_then_a_store_error_discards_nothing() {
    for lands in [false, true] {
        for len in [10_usize, 200_000] {
            let ctx = format!("lands {lands}, len {len}");
            // Store steps are the chunk writes, then the binds: the retry is the step after the first bind.
            let stages = if len as u64 <= SMALL_BLOB_MAX {
                0
            } else {
                len.div_ceil(MIN as usize)
            };
            let (store, blobs) = faulty(Faults {
                unknown_binds: 1,
                unknown_lands: lands,
                crash_at: Some(stages + 1),
                ..Faults::default()
            });
            let data = bytes(len, 12);
            let error = blobs.put_blob(&data[..], &options(MIN)).unwrap_err();
            assert!(error.is_bind_outcome_unknown(), "{ctx}: {error}");
            let calls = store.calls();
            let binds: Vec<_> = calls
                .iter()
                .filter_map(|c| match c {
                    Call::Bind { upload, .. } => Some(*upload),
                    _ => None,
                })
                .collect();
            // The retry went on past the store error, the same manifest each time.
            assert!(binds.len() > 2, "{ctx}: {calls:?}");
            assert!(binds.iter().all(|u| *u == binds[0]), "{ctx}");
            assert!(
                !calls.iter().any(|c| matches!(c, Call::Discard(_))),
                "{ctx}: {calls:?}"
            );
            if stages > 0 {
                assert_eq!(store.inner.staged_chunk_count(binds[0]), stages, "{ctx}");
            }
            // The first attempt's write landed or not: either way the store holds what it held.
            let bound = store.inner.bound_ids();
            assert_eq!(bound.contains(&blob_id_of(&data)), lands, "{ctx}");
            assert!(blobs.pin_set().pinned().is_empty(), "{ctx}");
        }
    }
}

/// `get_blob` of a blob stored with a chunk size of 1 (the encoding allows it, a writer does not, SPEC §9.1): it
/// reads back, a window of `WINDOW_MAX_CHUNKS` one-byte chunks at a time, and the manifest is looked up once, when
/// the reader opens (ADR-0051 item 4): every window reads by the manifest it keeps.
#[test]
fn get_blob_over_one_byte_chunks_counts_its_manifest_lookups() {
    use persia_blob::{BlobManifest, BlobRef};
    let len = 6400_usize;
    let data = bytes(len, 13);
    let store = FaultyStore::new(Faults::default());
    let upload = UploadId::new(99);
    let one = ChunkSize::from_encoding(1).unwrap();
    let mut crcs = Vec::new();
    let mut index = 0;
    persia_blob::chunk_all(&data[..], one, |chunk| {
        crcs.push(chunk.crc32c());
        store.inner.stage_chunk(upload, index, chunk.bytes())?;
        index += 1;
        Ok(())
    })
    .unwrap();
    let id = blob_id_of(&data);
    let manifest = BlobManifest::new(id, len as u64, one, upload, crcs).unwrap();
    assert!(matches!(
        store.inner.bind(&manifest).unwrap(),
        persia_blob::BindOutcome::Bound
    ));
    let r = BlobRef::new(
        id,
        len as u64,
        "application/octet-stream",
        1,
        std::collections::BTreeMap::default(),
    )
    .unwrap();
    let blobs = Blobs::new(Arc::clone(&store));
    assert_eq!(streamed(&blobs, &r, 1 << 16), data);
    let calls = store.calls();
    let lookups = calls
        .iter()
        .filter(|c| matches!(c, Call::Manifest { .. }))
        .count();
    let windows = calls
        .iter()
        .filter(|c| matches!(c, Call::ReadChunks(..)))
        .count();
    let window = WINDOW_MAX_CHUNKS as usize;
    assert_eq!(windows, len.div_ceil(window));
    assert_eq!(lookups, 1, "manifest lookups");
}

/// The `manifest` calls in `calls`, each with whether `Pinned::Blob` of that id was already pinned.
fn manifest_pins(calls: &[Call]) -> Vec<(BlobId, bool)> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::Manifest { blob, pinned } => Some((*blob, *pinned)),
            _ => None,
        })
        .collect()
}

/// Invariant 8: a stream pins its blob and the upload its manifest names while it lives (SPEC §9.5); a range read
/// holds no pin once it returns. The blob pin is taken before `manifest`, for both `get_blob` and `get_blob_range`.
#[test]
fn a_blob_stream_pins_its_blob_and_upload_until_dropped() {
    let data = bytes(2 * MIN as usize + 5, 4);
    let (store, blobs) = faulty(Faults::default());
    let (r, pins) = blobs.put_blob(&data[..], &options(MIN)).unwrap();
    let upload = upload_of(&pins);
    drop(pins);
    let set = blobs.pin_set();
    assert!(set.pinned().is_empty());
    let after_put = store.calls().len();

    let mut reader = blobs.get_blob(&r).unwrap();
    assert!(set.is_pinned(Pinned::Blob(r.blob_id())));
    assert!(set.is_pinned(Pinned::Upload(upload)));
    assert_eq!(
        manifest_pins(&store.calls()[after_put..]),
        vec![(r.blob_id(), true)],
        "get_blob"
    );
    let mut first = [0; 10];
    reader.read_exact(&mut first).unwrap();
    assert!(set.is_pinned(Pinned::Blob(r.blob_id())));
    drop(reader);
    assert!(set.pinned().is_empty());

    let after_stream = store.calls().len();
    assert_eq!(blobs.get_blob_range(&r, 3..9).unwrap(), &data[3..9]);
    assert_eq!(
        manifest_pins(&store.calls()[after_stream..]),
        vec![(r.blob_id(), true)],
        "get_blob_range"
    );
    assert!(set.pinned().is_empty());
    let missing = persia_blob::BlobRef::new(
        blob_id_of(b"absent"),
        6,
        r.mime(),
        r.chunk_size(),
        std::collections::BTreeMap::default(),
    )
    .unwrap();
    let after_range = store.calls().len();
    assert!(matches!(
        blobs.get_blob(&missing),
        Err(Error::BlobNotFound { .. })
    ));
    assert_eq!(
        manifest_pins(&store.calls()[after_range..]),
        vec![(missing.blob_id(), true)],
        "a failed open"
    );
    assert!(set.pinned().is_empty(), "a failed open releases its pin");
}

/// Bind attempts and backoff waits of a `put_blob` over a store poisoned by its first, unknown, bind (ROADMAP 9.4.7).
fn poisoned_put(len: usize, unknown_lands: bool) -> (Error, usize, Vec<Duration>, Vec<Call>) {
    let store = FaultyStore::new(Faults {
        unknown_binds: 1,
        unknown_lands,
        unknown_poisons: true,
        ..Faults::default()
    });
    let clock = Arc::new(ManualRetryClock::new());
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(11))
        .with_retry_clock(Arc::clone(&clock) as _);
    store.watch(blobs.pin_set());
    let error = blobs
        .put_blob(&bytes(len, 13)[..], &options(MIN))
        .unwrap_err();
    assert!(blobs.pin_set().pinned().is_empty());
    let calls = store.calls();
    let binds = calls
        .iter()
        .filter(|c| matches!(c, Call::Bind { .. }))
        .count();
    (error, binds, clock.sleeps(), calls)
}

/// Invariant 9: the first bind's outcome is unknown and poisons the store; the retry after one backoff wait is refused
/// as poisoned (a large blob's `bind` with an unknown outcome, a small blob's `bind_small` with a plain refusal of
/// its chunk write), so `put_blob` fails at once, still unknown, and discards nothing.
#[test]
fn a_poisoned_store_ends_the_bind_retry_at_once_as_unknown() {
    for lands in [false, true] {
        for len in [0_usize, 10, 200_000] {
            let ctx = format!("lands {lands}, len {len}");
            let (error, binds, sleeps, calls) = poisoned_put(len, lands);
            assert!(error.is_bind_outcome_unknown(), "{ctx}: {error}");
            assert!(error.is_store_poisoned(), "{ctx}: {error}");
            assert_eq!(binds, 2, "{ctx}: {calls:?}");
            assert_eq!(sleeps.len(), 1, "{ctx}: {sleeps:?}");
            assert!(
                !calls.iter().any(|c| matches!(c, Call::Discard(_))),
                "{ctx}: {calls:?}"
            );
        }
    }
}

/// Invariant 9: a bind refused as poisoned on its first attempt. The same id's outcome is unknown (an earlier
/// `put_blob` of the same blob poisoned the store): no wait, one attempt, unknown. Any other blob's refusal is a plain
/// store error: nothing was bound, and nothing more is retried.
#[test]
fn a_store_poisoned_before_the_first_attempt_is_not_retried() {
    let (store, blobs) = faulty(Faults {
        unknown_binds: 1,
        unknown_poisons: true,
        ..Faults::default()
    });
    let clock = Arc::new(ManualRetryClock::new());
    let blobs = blobs.with_retry_clock(Arc::clone(&clock) as _);
    // The empty blob stages nothing, so its bind reaches the store: the first poisons it, the second is unknown.
    let first = blobs.put_blob(&b""[..], &options(MIN)).unwrap_err();
    assert!(
        first.is_bind_outcome_unknown() && first.is_store_poisoned(),
        "{first}"
    );
    let sleeps = clock.sleeps().len();
    let again = blobs.put_blob(&b""[..], &options(MIN)).unwrap_err();
    assert!(
        again.is_bind_outcome_unknown() && again.is_store_poisoned(),
        "{again}"
    );
    assert_eq!(clock.sleeps().len(), sleeps, "no wait before giving up");
    // Another blob: a small one's chunk write and a large one's staging are refused plainly, not unknown.
    for len in [10_usize, 200_000] {
        let other = blobs
            .put_blob(&bytes(len, 14)[..], &options(MIN))
            .unwrap_err();
        assert!(matches!(other, Error::PackPoisoned), "{len}: {other}");
    }
    assert_eq!(clock.sleeps().len(), sleeps);
    assert!(!store.calls().iter().any(|c| matches!(c, Call::Discard(_))));
}

/// A `bind_budget` above the cap is a caller's error found before any byte is read; the cap itself is accepted
/// (SPEC §9.4: GC's grace period is at least twice this cap).
#[test]
fn a_bind_budget_above_the_cap_reads_nothing() {
    use persia::internal::blob::MAX_BIND_BUDGET;
    struct Untouchable;
    impl Read for Untouchable {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("put_blob read a blob it should have rejected");
        }
    }
    let (store, blobs) = faulty(Faults::default());
    let over = MAX_BIND_BUDGET + Duration::from_nanos(1);
    let opts = PutBlobOptions {
        bind_budget: over,
        ..PutBlobOptions::default()
    };
    assert!(matches!(
        blobs.put_blob(Untouchable, &opts),
        Err(Error::BindBudgetAboveMax { budget, max }) if budget == over && max == MAX_BIND_BUDGET
    ));
    assert!(store.calls().is_empty());
    assert!(blobs.pin_set().pinned().is_empty());
    let opts = PutBlobOptions {
        bind_budget: MAX_BIND_BUDGET,
        ..PutBlobOptions::default()
    };
    assert!(blobs.put_blob(&b"ok"[..], &opts).is_ok());
}

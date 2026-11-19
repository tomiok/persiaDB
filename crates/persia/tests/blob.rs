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
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(11));
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
            prop_assert!(store.calls().iter().any(|c| matches!(c, Call::Discard(_))));
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
                    Call::Bind { upload, blob, .. } => Some((*upload, *blob)),
                    _ => None,
                })
                .collect();
            // The same manifest each time: same upload, same id.
            assert_eq!(binds, vec![(upload, r.blob_id()); 4]);
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

#[test]
fn bind_attempts_that_all_stay_unknown_fail_and_discard_nothing() {
    let (store, blobs) = faulty(Faults {
        unknown_binds: u32::MAX,
        ..Faults::default()
    });
    let opts = PutBlobOptions {
        bind_attempts: 5,
        ..options(MIN)
    };
    let error = blobs.put_blob(&bytes(200_000, 6)[..], &opts).unwrap_err();
    assert!(error.is_bind_outcome_unknown());
    let calls = store.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|c| matches!(c, Call::Bind { .. }))
            .count(),
        5
    );
    assert!(!calls.iter().any(|c| matches!(c, Call::Discard(_))));
    assert!(blobs.pin_set().pinned().is_empty());
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
    assert_eq!(set.unless_pinned(id, || ()), None);
    drop(b);
    assert!(!set.is_pinned(id));
    assert_eq!(set.unless_pinned(id, || 1), Some(1));
}

/// The sweep's check and delete are one step: a pin asked for while the delete runs waits for it.
#[test]
fn a_pin_waits_for_a_delete_under_the_lock() {
    let set = Arc::new(PinSet::new());
    let id = Pinned::Upload(UploadId::new(3));
    let (tx, rx) = mpsc::channel();
    let pinner = Arc::clone(&set);
    let ran = set.unless_pinned(id, || {
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

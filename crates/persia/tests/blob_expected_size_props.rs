//! `PutBlobOptions::expected_size` at its boundaries, with and without a resume (SPEC §9.4, §9.6; ROADMAP 9.3.7).
//!
//! Every case draws a blob length around the small-blob bound and the chunk size (0, 1, 64 KiB, 64 KiB + 1, several
//! chunks), an `expected_size` (none, exact, one more, one less, 0, `u64::MAX`), an `expected_id` (none, right,
//! wrong) and a resume (none, or a token whose upload staged 0, 1 or every chunk of the blob), and checks the outcome
//! against an oracle:
//! 1. A resume with neither `expected_id` nor `expected_size` is `ResumeUnchecked`, before anything is read or
//!    written; an `expected_size` of `Some(0)` counts as given.
//! 2. Otherwise a wrong `expected_id` is `BlobIdMismatch` (checked first), then an `expected_size` that is not the
//!    length is `BlobSizeMismatch { expected, actual }`, neither of them corruption.
//! 3. A mismatch binds nothing and leaves no pin; a new upload leaves no key at all under `blobs/` (its chunks are
//!    discarded; a small blob stages none), and a resume leaves only chunks under the token's upload (never discarded:
//!    it may be bound to another blob, ADR-0053), so the token still resumes to the blob afterwards.
//! 4. Otherwise the blob binds the reference an uninterrupted `put_blob` of it binds, and reads back.
//! 5. On the pack (which cannot resume) the same checks of a new upload: a mismatch leaves the blob unbound.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure. Not run under Miri (SHA-256 of megabytes, real
//! files for the pack).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::manual_assert_eq
)] // Tests: inputs are generated; `assert!(a == b)` keeps blob bytes out of a failure message.

use core::num::NonZeroUsize;
use std::collections::BTreeSet;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::BlobDatabase;
use persia::internal::retry::ManualRetryClock;
use persia_blob::remote::RemoteBlobStore;
use persia_blob::{
    BlobId, BlobStore, ChunkSize, Error, ResumeToken, SMALL_BLOB_MAX, UploadId, blob_id_of,
};
use persia_engine::db::{DatabaseOptions, SegmentIds};
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::{KeyPrefix, MemoryStorage, Storage};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

const CHUNK: u32 = ChunkSize::MIN;
const CHUNK_LEN: usize = CHUNK as usize;
const TOKEN: u128 = 0x00e5_17e5;

type Remote = RemoteBlobStore<MemoryStorage>;

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed) ^ ((i >> 11) as u8))
        .collect()
}

fn remote(seed: u64) -> Blobs<Remote> {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_hours(1_000)));
    let storage = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    let store = Arc::new(RemoteBlobStore::new(storage).with_parallelism(NonZeroUsize::MIN));
    Blobs::with_upload_ids(store, SegmentIds::seeded(seed))
        .with_retry_clock(Arc::new(ManualRetryClock::new()))
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(CHUNK).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn token() -> ResumeToken {
    ResumeToken {
        upload_id: UploadId::new(TOKEN),
        chunk_size: ChunkSize::new(CHUNK).unwrap(),
    }
}

fn keys(store: &Remote) -> BTreeSet<String> {
    store
        .storage()
        .list(&KeyPrefix::new("blobs/").unwrap())
        .unwrap()
        .into_iter()
        .map(|m| m.key.as_str().to_owned())
        .collect()
}

/// A reader of `data` that counts its reads.
struct Counting<'a> {
    data: &'a [u8],
    reads: usize,
}

impl Read for Counting<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        self.data.read(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Size {
    None,
    Exact,
    Plus1,
    Minus1,
    Zero,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Id {
    None,
    Right,
    Wrong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kept {
    None,
    One,
    All,
}

#[derive(Debug, Clone)]
struct Case {
    len: usize,
    size: Size,
    id: Id,
    /// `None`: a new upload; else a resume of a token whose upload staged that many of the blob's chunks.
    resume: Option<Kept>,
}

impl Case {
    fn expected_size(&self) -> Option<u64> {
        let len = self.len as u64;
        match self.size {
            Size::None => None,
            Size::Exact => Some(len),
            Size::Plus1 => Some(len + 1),
            // One less than 0 does not exist: the largest size instead.
            Size::Minus1 => Some(len.checked_sub(1).unwrap_or(u64::MAX)),
            Size::Zero => Some(0),
            Size::Max => Some(u64::MAX),
        }
    }

    fn expected_id(&self, data: &[u8]) -> Option<BlobId> {
        match self.id {
            Id::None => None,
            Id::Right => Some(blob_id_of(data)),
            Id::Wrong => Some(blob_id_of(b"not the blob")),
        }
    }

    fn options(&self, data: &[u8]) -> PutBlobOptions {
        PutBlobOptions {
            expected_size: self.expected_size(),
            expected_id: self.expected_id(data),
            resume: self.resume.map(|_| token()),
            ..options()
        }
    }

    /// The oracle: the error the put must fail with, `None` when it binds.
    fn verdict(&self, data: &[u8]) -> Option<Verdict> {
        let len = data.len() as u64;
        if self.resume.is_some() && self.id == Id::None && self.size == Size::None {
            return Some(Verdict::Unchecked);
        }
        // A reader longer than the size is cut one byte past it: a size mismatch, before the id (a prefix's).
        if let Some(s) = self.expected_size()
            && s < len
        {
            return Some(Verdict::SizeMismatch(s));
        }
        if self.id == Id::Wrong {
            return Some(Verdict::IdMismatch);
        }
        match self.expected_size() {
            Some(s) if s != len => Some(Verdict::SizeMismatch(s)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Unchecked,
    IdMismatch,
    SizeMismatch(u64),
}

fn check_error(case: &Case, data: &[u8], verdict: Verdict, e: &Error) {
    let len = data.len() as u64;
    match verdict {
        Verdict::Unchecked => assert!(matches!(e, Error::ResumeUnchecked), "{case:?}: {e:?}"),
        Verdict::IdMismatch => assert!(
            matches!(e, Error::BlobIdMismatch { actual, .. } if *actual == blob_id_of(data)),
            "{case:?}: {e:?}"
        ),
        // Longer: what was read, past `s` (one byte past it, or a resume's longer kept run), never more than the blob.
        Verdict::SizeMismatch(s) if s < len => assert!(
            matches!(e, Error::BlobSizeMismatch { expected, actual } if *expected == s && *actual > s && *actual <= len),
            "{case:?}: {e:?}"
        ),
        Verdict::SizeMismatch(s) => assert!(
            matches!(e, Error::BlobSizeMismatch { expected, actual } if *expected == s && *actual == len),
            "{case:?}: {e:?}"
        ),
    }
    assert!(!e.is_corruption(), "{case:?}");
    assert!(!e.is_bind_outcome_unknown(), "{case:?}");
}

fn lens() -> Vec<usize> {
    let small = SMALL_BLOB_MAX as usize;
    vec![
        0,
        1,
        small - 1,
        small,
        small + 1,
        2 * CHUNK_LEN,
        3 * CHUNK_LEN + 7,
    ]
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::sample::select(lens()),
        prop::sample::select(vec![
            Size::None,
            Size::Exact,
            Size::Plus1,
            Size::Minus1,
            Size::Zero,
            Size::Max,
        ]),
        prop::sample::select(vec![Id::None, Id::Right, Id::Wrong]),
        prop::sample::select(vec![
            None,
            Some(Kept::None),
            Some(Kept::One),
            Some(Kept::All),
        ]),
    )
        .prop_map(|(len, size, id, resume)| Case {
            len,
            size,
            id,
            resume,
        })
}

fn run_remote(case: &Case) {
    let data = bytes(case.len, case.len as u8);
    let blobs = remote(7);
    let store = blobs.store();
    let chunks: Vec<&[u8]> = data.chunks(CHUNK_LEN).collect();
    let kept = match case.resume {
        None | Some(Kept::None) => 0,
        Some(Kept::One) => chunks.len().min(1),
        Some(Kept::All) => chunks.len(),
    };
    for (index, chunk) in (0..).zip(chunks.iter().take(kept)) {
        store.stage_chunk(token().upload_id, index, chunk).unwrap();
    }
    let before = keys(store);
    let mut reader = Counting {
        data: &data,
        reads: 0,
    };
    let result = blobs.put_blob(&mut reader, &case.options(&data));
    let uninterrupted = remote(99).put_blob(&data[..], &options()).unwrap().0;
    match (case.verdict(&data), result) {
        (None, Ok((r, pins))) => {
            assert_eq!(r, uninterrupted, "{case:?}");
            drop(pins);
            assert!(
                blobs.get_blob_range(&r, 0..r.size()).unwrap() == data,
                "{case:?}"
            );
        }
        (None, Err(e)) => panic!("{case:?}: {e:?}"),
        (Some(v), Ok(_)) => panic!("{case:?}: bound, not {v:?}"),
        (Some(v), Err(e)) => {
            check_error(case, &data, v, &e);
            assert!(
                store.manifest(blob_id_of(&data)).unwrap().is_none(),
                "{case:?}: bound"
            );
            assert!(
                blobs.pin_set().pinned().is_empty(),
                "{case:?}: a pin is left"
            );
            let after = keys(store);
            if v == Verdict::Unchecked {
                assert_eq!(reader.reads, 0, "{case:?}: read before refusing");
                assert_eq!(after, before, "{case:?}");
            } else if case.resume.is_none() {
                assert_eq!(after, before, "{case:?}: a new upload left keys");
            } else {
                let prefix = format!("blobs/uploads/{TOKEN:032x}/");
                let added: Vec<&String> = after.difference(&before).collect();
                assert!(
                    before.is_subset(&after),
                    "{case:?}: the token's chunks went"
                );
                assert!(
                    added.iter().all(|k| k.starts_with(&prefix)),
                    "{case:?}: left outside the token's upload: {added:?}"
                );
                assert!(
                    after.iter().all(|k| k.starts_with("blobs/uploads/")),
                    "{case:?}: a manifest is left: {after:?}"
                );
            }
            if case.resume.is_some() {
                // The token is as good as it was.
                let checked = PutBlobOptions {
                    expected_size: Some(data.len() as u64),
                    resume: Some(token()),
                    ..options()
                };
                let (r, _pins) = blobs.put_blob(&data[..], &checked).unwrap();
                assert_eq!(r, uninterrupted, "{case:?}: the token no longer resumes");
                assert!(
                    blobs.get_blob_range(&r, 0..r.size()).unwrap() == data,
                    "{case:?}"
                );
            }
        }
    }
}

fn run_pack(case: &Case) {
    let dir = tempfile::tempdir().unwrap();
    let db =
        BlobDatabase::create(dir.path().join("app.persia"), DatabaseOptions::default()).unwrap();
    let data = bytes(case.len, case.len as u8);
    let new_upload = Case {
        resume: None,
        ..case.clone()
    };
    let result = db.blobs().put_blob(&data[..], &new_upload.options(&data));
    match (new_upload.verdict(&data), result) {
        (None, Ok((r, _pins))) => {
            assert!(
                db.blobs().get_blob_range(&r, 0..r.size()).unwrap() == data,
                "{case:?}"
            );
        }
        (None, Err(e)) => panic!("pack, {case:?}: {e:?}"),
        (Some(v), Ok(_)) => panic!("pack, {case:?}: bound, not {v:?}"),
        (Some(v), Err(e)) => {
            check_error(&new_upload, &data, v, &e);
            assert!(
                db.blobs()
                    .store()
                    .manifest(blob_id_of(&data))
                    .unwrap()
                    .is_none(),
                "pack, {case:?}"
            );
            assert!(db.blobs().pin_set().pinned().is_empty(), "pack, {case:?}");
        }
    }
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 1500 } else { 24 },
        rng_seed: RngSeed::Fixed(9307),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn expected_size_binds_exactly_the_blob_of_that_length(case in case()) {
        run_remote(&case);
        if case.resume.is_none() {
            run_pack(&case);
        }
    }
}

/// The boundaries every run covers, whatever the sample: each size against the empty blob, the small-blob bound and
/// a chunked blob, on a new upload and on a resume that kept every chunk (the restage of ROADMAP 9.3.6).
#[test]
fn every_size_boundary_on_the_edges() {
    let small = SMALL_BLOB_MAX as usize;
    for len in [0, small, small + 1] {
        for size in [
            Size::None,
            Size::Exact,
            Size::Plus1,
            Size::Minus1,
            Size::Zero,
            Size::Max,
        ] {
            for resume in [None, Some(Kept::All)] {
                run_remote(&Case {
                    len,
                    size,
                    id: Id::None,
                    resume,
                });
            }
        }
    }
}

/// `expected_size: Some(0)` is a size given: a resume of the empty blob with it binds, and a resume of a longer blob
/// with it is a size mismatch, not `ResumeUnchecked`.
#[test]
fn a_zero_expected_size_is_given_not_absent() {
    for (len, verdict) in [(0, None), (1, Some(Verdict::SizeMismatch(0)))] {
        let case = Case {
            len,
            size: Size::Zero,
            id: Id::None,
            resume: Some(Kept::None),
        };
        let data = bytes(len, 1);
        assert_eq!(case.verdict(&data), verdict);
        run_remote(&case);
    }
}

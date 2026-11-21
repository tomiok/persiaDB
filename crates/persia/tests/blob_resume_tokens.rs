//! Resume tokens through `put_blob` (SPEC §9.4, §9.5, §9.6; ROADMAP 9.3.4, 9.3.6; ADR-0053), over the remote layout on
//! a clocked `MemoryStorage`, whose keys are listed exactly, and over the pack.
//!
//! Invariants:
//! 1. A remote upload whose reader failed after a window was staged, resumed with the token and the whole blob, binds
//!    the `BlobRef` an uninterrupted `put_blob` binds under the token's upload; the kept chunks are not written again
//!    (their `last_modified` is unchanged), and `blobs/` holds exactly the manifest and the upload's chunks.
//! 2. A resume on the pack is `ResumeUnsupported`: nothing is read from the reader, nothing is pinned, and the pack
//!    file is unchanged (absent before the first blob, the same bytes after it).
//! 3. A token holds only an upload id and a chunk size, so an unknown upload id and a token of another prefix or
//!    bucket are one case. An empty listing starts a fresh upload under the token's id (a blob of at most 64 KiB is
//!    stored chunked, not inline). Chunks under that id holding other bytes at an index the resume writes, with none
//!    it keeps, conflict: the upload moves to a new id and leaves them as they were. Full chunks of the token's size
//!    that are not the reader's bytes are `InvalidResume`, and nothing is staged.
//! 4. A token chunk size below the floor fails before anything is read, staged or pinned.
//! 5. A token whose chunk size is not the one the upload was staged with keeps no chunk (none is of the token's
//!    length), conflicts at chunk 0 and binds the reader's blob, at the token's chunk size, under a new upload id;
//!    GC past the grace period then takes the old chunks.
//! 6. After failed resumes (one that stages nothing, one that fails after a conflict restage, and a replayed stale
//!    token that restages again) and the resume that binds, a GC within the grace period deletes nothing, and one
//!    past it leaves exactly the bound blob's manifest and its chunks under `blobs/`.
//!
//! Deterministic: fixed data, seeded upload ids, a manual clock, no sleeps.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::manual_assert_eq,
    clippy::assert_is_empty
)] // Tests: inputs are fixed; `assert!(a == b)` keeps megabytes of blob bytes out of a failure message.

use core::num::NonZeroUsize;
use core::ops::Range;
use std::collections::BTreeSet;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::{Blob, BlobDatabase};
use persia::internal::gc::{DEFAULT_GRACE, GcOptions, GcReport};
use persia::internal::retry::ManualRetryClock;
use persia_blob::remote::{RemoteBlobStore, RemoteKey};
use persia_blob::{BlobId, BlobRef, BlobStore, ChunkSize, Error, ResumeToken, UploadId};
use persia_engine::db::{Database, DatabaseOptions, SegmentIds};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::{KeyPrefix, MemoryStorage, Storage};

const CHUNK: u32 = ChunkSize::MIN;
const CHUNK_LEN: usize = CHUNK as usize;
/// Chunks `put_blob` stages per window at 64 KiB (`WINDOW_BYTES / chunk size`).
const WINDOW: usize = 64;

type Remote = RemoteBlobStore<MemoryStorage>;

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ ((i >> 13) as u8))
        .collect()
}

fn size(chunk: u32) -> ChunkSize {
    ChunkSize::new(chunk).unwrap()
}

fn options(chunk: u32) -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: size(chunk),
        ..PutBlobOptions::default()
    }
}

fn resume(token: ResumeToken) -> PutBlobOptions {
    PutBlobOptions {
        resume: Some(token),
        ..options(CHUNK)
    }
}

fn token(upload: u128, chunk: u32) -> ResumeToken {
    ResumeToken {
        upload_id: UploadId::new(upload),
        chunk_size: size(chunk),
    }
}

/// The writer's handle on a remote store over a clocked `MemoryStorage`, one request at a time, and the clock.
fn remote(seed: u64) -> (Arc<ManualClock>, Blobs<Remote>) {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_hours(1_000)));
    let storage = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    let store = Arc::new(RemoteBlobStore::new(storage).with_parallelism(NonZeroUsize::MIN));
    let blobs = Blobs::with_upload_ids(store, SegmentIds::seeded(seed))
        .with_retry_clock(Arc::new(ManualRetryClock::new()));
    (clock, blobs)
}

/// [`remote`] under a document log with collection `c`.
fn database(dir: &Path, seed: u64) -> (Arc<ManualClock>, BlobDatabase<Remote>) {
    let (clock, blobs) = remote(seed);
    let mut log = Database::create(dir.join("app.persia"), DatabaseOptions::default()).unwrap();
    log.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    (clock, BlobDatabase::new(log, blobs))
}

fn gc_at(db: &BlobDatabase<Remote>, clock: &ManualClock) -> GcReport {
    db.gc(&GcOptions {
        now: Some(clock.now()),
        ..GcOptions::default()
    })
    .unwrap()
}

/// The reference an uninterrupted `put_blob` returns, on a store of its own.
fn uninterrupted(data: &[u8], chunk: u32) -> BlobRef {
    remote(99).1.put_blob(data, &options(chunk)).unwrap().0
}

fn interruption(e: Error) -> (ResumeToken, Error) {
    match e {
        Error::UploadInterrupted { token, source } => (token, *source),
        other => panic!("not an interrupted upload: {other:?}"),
    }
}

/// Every key under `blobs/`.
fn keys(store: &Remote) -> BTreeSet<String> {
    store
        .storage()
        .list(&KeyPrefix::new("blobs/").unwrap())
        .unwrap()
        .into_iter()
        .map(|meta| meta.key.as_str().to_owned())
        .collect()
}

fn manifest_key(blob_id: BlobId) -> String {
    RemoteKey::Manifest(blob_id)
        .key()
        .unwrap()
        .as_str()
        .to_owned()
}

fn chunk_keys(upload_id: UploadId, indices: Range<u32>) -> BTreeSet<String> {
    indices
        .map(|index| {
            RemoteKey::Chunk { upload_id, index }
                .key()
                .unwrap()
                .as_str()
                .to_owned()
        })
        .collect()
}

/// The upload the blob `r` is bound to, and its keys: the manifest (plain, not inline) and every chunk.
fn bound_keys(store: &Remote, r: &BlobRef) -> (UploadId, BTreeSet<String>) {
    let m = store.manifest(r.blob_id()).unwrap().unwrap();
    assert!(!m.is_inline(), "a resume stores its blob chunked");
    let mut keys = chunk_keys(m.upload_id(), 0..m.chunk_count());
    keys.insert(manifest_key(r.blob_id()));
    (m.upload_id(), keys)
}

fn read_back(blobs: &Blobs<impl BlobStore + ?Sized>, r: &BlobRef, data: &[u8]) {
    let mut streamed = Vec::new();
    blobs
        .get_blob(r)
        .unwrap()
        .read_to_end(&mut streamed)
        .unwrap();
    assert!(streamed == data, "streamed other bytes");
}

/// A reader of `data` that fails once it has yielded `fail_at` bytes.
struct FailAfter<'a> {
    data: &'a [u8],
    pos: usize,
    fail_at: usize,
}

impl Read for FailAfter<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.fail_at {
            return Err(io::Error::other("the reader failed"));
        }
        let end = self.fail_at.min(self.data.len()).min(self.pos + out.len());
        let n = end - self.pos;
        out[..n].copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(n)
    }
}

fn fail_in(data: &[u8], chunk: usize) -> FailAfter<'_> {
    FailAfter {
        data,
        pos: 0,
        fail_at: chunk * CHUNK_LEN + 9,
    }
}

/// A reader of `data` that counts its `read` calls.
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

// -- 1. a successful remote resume --

#[test]
fn an_interrupted_remote_upload_resumes_through_put_blob_without_rewriting_its_kept_chunks() {
    let (clock, blobs) = remote(1);
    let store = blobs.store();
    let data = bytes((WINDOW + 5) * CHUNK_LEN + 100, 1);
    let (token, source) = interruption(
        blobs
            .put_blob(fail_in(&data, WINDOW + 2), &options(CHUNK))
            .unwrap_err(),
    );
    assert!(matches!(source, Error::Read(_)), "{source:?}");
    assert_eq!(token.chunk_size, size(CHUNK));
    assert_eq!(
        keys(store),
        chunk_keys(token.upload_id, 0..WINDOW as u32),
        "the first window, staged before the reader failed"
    );
    assert!(blobs.pin_set().pinned().is_empty());
    let staged_at = clock.now();
    clock.advance(Duration::from_secs(60));

    let (r, pins) = blobs.put_blob(&data[..], &resume(token)).unwrap();
    assert_eq!(r, uninterrupted(&data, CHUNK));
    let (upload, expected) = bound_keys(store, &r);
    assert_eq!(upload, token.upload_id, "bound under the token's upload");
    assert_eq!(expected.len(), WINDOW + 6 + 1);
    assert_eq!(keys(store), expected);
    // The kept run was read from the caller and checked, not written again: only the chunks after it are new.
    for meta in store
        .storage()
        .list(&KeyPrefix::new("blobs/uploads/").unwrap())
        .unwrap()
    {
        let Some(RemoteKey::Chunk { index, .. }) = RemoteKey::parse(&meta.key) else {
            panic!("not a chunk: {}", meta.key);
        };
        let written = if (index as usize) < WINDOW {
            staged_at
        } else {
            clock.now()
        };
        assert_eq!(meta.last_modified, Some(written), "chunk {index}");
    }
    read_back(&blobs, &r, &data);
    drop(pins);
    assert!(blobs.pin_set().pinned().is_empty());
}

// -- 2. the pack cannot resume --

#[test]
fn a_resume_on_the_pack_reads_nothing_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db =
        BlobDatabase::create(dir.path().join("app.persia"), DatabaseOptions::default()).unwrap();
    let pack = dir.path().join("app.persia.blobs");
    let blobs = db.blobs();
    let data = bytes(3 * CHUNK_LEN + 100, 2);
    let rejected = |t: ResumeToken| {
        let mut reader = Counting {
            data: &data,
            reads: 0,
        };
        let e = blobs.put_blob(&mut reader, &resume(t)).unwrap_err();
        assert!(matches!(e, Error::ResumeUnsupported), "{e:?}");
        assert_eq!(reader.reads, 0, "the reader was read");
        assert!(blobs.pin_set().pinned().is_empty());
    };

    // Before the first blob there is no pack, and a resume does not create one.
    assert!(!pack.exists());
    rejected(token(7, CHUNK));
    assert!(!pack.exists(), "a rejected resume created the pack");

    // With a blob in it: a resume of that blob's own upload, or of another, leaves the pack's bytes as they were.
    let (stored, pins) = blobs.put_blob(&data[..], &options(CHUNK)).unwrap();
    drop(pins);
    let upload = blobs
        .store()
        .manifest(stored.blob_id())
        .unwrap()
        .unwrap()
        .upload_id();
    let before = std::fs::read(&pack).unwrap();
    for t in [
        token(upload.get(), CHUNK),
        token(7, CHUNK),
        token(7, 2 * CHUNK),
    ] {
        rejected(t);
        assert!(std::fs::read(&pack).unwrap() == before, "the pack changed");
    }
    assert!(blobs.get_blob_range(&stored, 0..stored.size()).unwrap() == data);
}

// -- 3. unknown and foreign tokens --

#[test]
fn an_unknown_upload_id_starts_a_fresh_upload_under_it() {
    // A token of another prefix or bucket lists nothing here either: the same case.
    for len in [1, 1000, CHUNK_LEN, CHUNK_LEN + 1, 3 * CHUNK_LEN + 100] {
        let (_, blobs) = remote(3);
        let store = blobs.store();
        let data = bytes(len, 3);
        let t = token(0xfeed_0000 + len as u128, CHUNK);
        let (r, _pins) = blobs.put_blob(&data[..], &resume(t)).unwrap();
        assert_eq!(r, uninterrupted(&data, CHUNK), "{len}");
        let (upload, expected) = bound_keys(store, &r);
        assert_eq!(upload, t.upload_id, "{len}: under the token's id");
        assert_eq!(keys(store), expected, "{len}");
        read_back(&blobs, &r, &data);
    }
}

#[test]
fn a_token_whose_id_holds_other_bytes_moves_the_upload_to_a_new_id() {
    let data = bytes(3 * CHUNK_LEN + 100, 4);
    let foreign = bytes(4 * CHUNK_LEN, 5);
    // Other bytes at an index the resume writes, and none it keeps: chunks of another chunk size (another prefix's
    // upload at 128 KiB), or a short chunk 0 (a one-chunk upload's).
    let cases: [(&str, Vec<&[u8]>); 2] = [
        (
            "chunks of another size",
            foreign.chunks(2 * CHUNK_LEN).collect(),
        ),
        ("a short chunk 0", vec![&foreign[..100]]),
    ];
    for (what, chunks) in cases {
        let (_, blobs) = remote(4);
        let store = blobs.store();
        let t = token(0x00c0_111d, CHUNK);
        for (index, chunk) in (0..).zip(&chunks) {
            store.stage_chunk(t.upload_id, index, chunk).unwrap();
        }
        let theirs = keys(store);
        let (r, _pins) = blobs.put_blob(&data[..], &resume(t)).unwrap();
        assert_eq!(r, uninterrupted(&data, CHUNK), "{what}");
        let (upload, mut expected) = bound_keys(store, &r);
        assert_ne!(upload, t.upload_id, "{what}: moved to a new upload id");
        expected.extend(theirs);
        assert_eq!(
            keys(store),
            expected,
            "{what}: only the new upload is written"
        );
        for (index, chunk) in (0..).zip(&chunks) {
            let key = RemoteKey::Chunk {
                upload_id: t.upload_id,
                index,
            }
            .key()
            .unwrap();
            assert!(
                store.storage().get(&key).unwrap() == *chunk,
                "{what}: chunk {index} changed"
            );
        }
        read_back(&blobs, &r, &data);
    }
}

#[test]
fn an_upload_a_resume_restaged_to_is_discarded_on_a_mismatch_or_a_duplicate() {
    // The token's upload holds a short chunk 0 of other bytes, so the resume restages to an upload of its own, which
    // it may discard; never the token's (ADR-0053 item 11).
    let data = bytes(3 * CHUNK_LEN + 100, 12);
    let t = token(0x0d15_ca2d, CHUNK);
    let collided = |blobs: &Blobs<Remote>| {
        blobs
            .store()
            .stage_chunk(t.upload_id, 0, &bytes(100, 13))
            .unwrap();
    };

    // An `expected_id` the bytes do not hash to: nothing bound, the restaged upload discarded.
    let (_, blobs) = remote(12);
    collided(&blobs);
    let theirs = keys(blobs.store());
    let mismatch = PutBlobOptions {
        expected_id: Some(persia_blob::blob_id_of(b"another blob")),
        ..resume(t)
    };
    let e = blobs.put_blob(&data[..], &mismatch).unwrap_err();
    assert!(matches!(e, Error::BlobIdMismatch { .. }), "{e:?}");
    assert_eq!(keys(blobs.store()), theirs);
    assert!(blobs.pin_set().pinned().is_empty());

    // The blob bound already, under its own upload: a duplicate, and the restaged upload discarded.
    let (_, blobs) = remote(13);
    let (first, _first_pins) = blobs.put_blob(&data[..], &options(CHUNK)).unwrap();
    collided(&blobs);
    let before = keys(blobs.store());
    let (r, _pins) = blobs.put_blob(&data[..], &resume(t)).unwrap();
    assert_eq!(r, first);
    assert_eq!(
        keys(blobs.store()),
        before,
        "the duplicate's upload is gone"
    );
}

#[test]
fn a_token_whose_id_holds_full_chunks_of_another_blob_is_a_callers_error() {
    let (_, blobs) = remote(5);
    let store = blobs.store();
    let data = bytes(3 * CHUNK_LEN + 100, 6);
    let foreign = bytes(2 * CHUNK_LEN, 7);
    let t = token(0xf0e1, CHUNK);
    for (index, chunk) in (0..).zip(foreign.chunks(CHUNK_LEN)) {
        store.stage_chunk(t.upload_id, index, chunk).unwrap();
    }
    let before = keys(store);
    let e = blobs.put_blob(&data[..], &resume(t)).unwrap_err();
    assert!(matches!(e, Error::InvalidResume { .. }), "{e:?}");
    assert_eq!(keys(store), before, "nothing is staged");
    assert!(blobs.pin_set().pinned().is_empty());
    // A new upload stores the blob.
    let (r, _pins) = blobs.put_blob(&data[..], &options(CHUNK)).unwrap();
    assert_eq!(r, uninterrupted(&data, CHUNK));
}

// -- 4. a token chunk size below the floor --

#[test]
fn a_token_chunk_size_below_the_floor_reads_stages_and_pins_nothing() {
    let (_, blobs) = remote(6);
    let store = blobs.store();
    let data = bytes(2 * CHUNK_LEN, 8);
    let staged = token(0xab, CHUNK);
    for (index, chunk) in (0..).zip(data.chunks(CHUNK_LEN)) {
        store.stage_chunk(staged.upload_id, index, chunk).unwrap();
    }
    let before = keys(store);
    for below in [1, 4096, CHUNK - 1] {
        let t = ResumeToken {
            upload_id: staged.upload_id,
            chunk_size: ChunkSize::from_encoding(below).unwrap(),
        };
        let mut reader = Counting {
            data: &data,
            reads: 0,
        };
        let e = blobs.put_blob(&mut reader, &resume(t)).unwrap_err();
        assert!(
            matches!(e, Error::ChunkSizeBelowMin { chunk_size, min: CHUNK } if chunk_size == below),
            "{e:?}"
        );
        assert_eq!(reader.reads, 0, "{below}: the reader was read");
        assert_eq!(keys(store), before, "{below}");
        assert!(blobs.pin_set().pinned().is_empty(), "{below}");
    }
}

// -- 5. a token of another chunk size than the upload's --

#[test]
fn a_token_of_another_chunk_size_restarts_under_a_new_id_and_gc_takes_the_old_chunks() {
    for (staged_at, token_at) in [(2 * CHUNK, CHUNK), (CHUNK, 2 * CHUNK)] {
        let what = format!("staged at {staged_at}, resumed at {token_at}");
        let dir = tempfile::tempdir().unwrap();
        let (clock, mut db) = database(dir.path(), 7);
        let data = bytes(5 * CHUNK_LEN + 100, 9);
        // What an interrupted `put_blob` at `staged_at` left: its first two chunks.
        let old = UploadId::new(0x01d);
        for (index, chunk) in (0..).zip(data.chunks(staged_at as usize).take(2)) {
            db.blobs().store().stage_chunk(old, index, chunk).unwrap();
        }
        let theirs = chunk_keys(old, 0..2);
        let t = ResumeToken {
            upload_id: old,
            chunk_size: size(token_at),
        };
        let blob = Blob::from_reader(&data[..]).with_options(resume(t));
        let r = db
            .put("c", &Document::new("d"), blob, Durability::Fsync)
            .unwrap();
        db.release_durable();
        assert_eq!(r, uninterrupted(&data, token_at), "{what}");
        assert_eq!(r.chunk_size(), token_at, "{what}: the token's chunk size");
        let store = db.blobs().store();
        let (upload, bound) = bound_keys(store, &r);
        assert_ne!(upload, old, "{what}: chunk 0 conflicts, so a new upload id");
        assert_eq!(
            keys(store),
            bound.union(&theirs).cloned().collect(),
            "{what}: the old chunks are left to GC"
        );
        read_back(db.blobs(), &r, &data);

        clock.advance(DEFAULT_GRACE + Duration::from_hours(1));
        let report = gc_at(&db, &clock);
        assert_eq!(report.sweep.uploads_deleted, 1, "{what}: {report:?}");
        assert_eq!(report.sweep.blobs_deleted, 0, "{what}: {report:?}");
        assert_eq!(keys(store), bound, "{what}: GC took the old chunks");
        read_back(db.blobs(), &r, &data);
    }
}

// -- 6. no orphaned bytes after failed resumes --

#[test]
fn failed_resumes_leave_nothing_after_gc_but_the_bound_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, mut db) = database(dir.path(), 8);
    let data = bytes((2 * WINDOW + 5) * CHUNK_LEN + 100, 10);
    let window = WINDOW as u32;
    let (t0, t1, t2) = {
        let blobs = db.blobs();
        let store = blobs.store();
        // The first `put_blob` stages the first window, then its reader fails.
        let (t0, _) = interruption(
            blobs
                .put_blob(fail_in(&data, WINDOW + 3), &options(CHUNK))
                .unwrap_err(),
        );
        // A resume whose reader fails inside the next window, before it is staged: the same token, nothing new.
        let (again, source) = interruption(
            blobs
                .put_blob(fail_in(&data, WINDOW + 10), &resume(t0))
                .unwrap_err(),
        );
        assert_eq!(again, t0);
        assert!(matches!(source, Error::Read(_)), "{source:?}");
        assert_eq!(keys(store), chunk_keys(t0.upload_id, 0..window));
        // Other bytes at chunk `WINDOW + 1` of the token's upload, short, so no resume keeps them.
        store
            .stage_chunk(t0.upload_id, window + 1, &bytes(100, 11))
            .unwrap();
        // A resume that conflicts there restages to a new upload, stages the window there, then its reader fails.
        let (t1, source) = interruption(
            blobs
                .put_blob(fail_in(&data, 2 * WINDOW + 2), &resume(t0))
                .unwrap_err(),
        );
        assert_ne!(t1.upload_id, t0.upload_id);
        assert!(matches!(source, Error::Read(_)), "{source:?}");
        // The stale token, replayed: it conflicts and restages again, to a third upload, and fails the same way.
        let (t2, _) = interruption(
            blobs
                .put_blob(fail_in(&data, 2 * WINDOW + 2), &resume(t0))
                .unwrap_err(),
        );
        assert!(t2.upload_id != t0.upload_id && t2.upload_id != t1.upload_id);
        assert!(
            blobs.pin_set().pinned().is_empty(),
            "every pin dropped with its error"
        );
        (t0, t1, t2)
    };

    // The upload `t1` names resumes, referenced by a document.
    let blob = Blob::from_reader(&data[..]).with_options(resume(t1));
    let r = db
        .put("c", &Document::new("d"), blob, Durability::Fsync)
        .unwrap();
    db.release_durable();
    assert!(db.blobs().pin_set().pinned().is_empty());
    assert_eq!(r, uninterrupted(&data, CHUNK));
    let store = db.blobs().store();
    let (upload, bound) = bound_keys(store, &r);
    assert_eq!(upload, t1.upload_id);
    let mut all = bound.clone();
    all.extend(chunk_keys(t0.upload_id, 0..window + 2));
    all.extend(chunk_keys(t2.upload_id, 0..2 * window + 1));
    assert_eq!(
        keys(store),
        all,
        "the failed uploads' chunks are still there"
    );

    // Within the grace period they stay: a token of theirs could still resume.
    let report = gc_at(&db, &clock);
    assert_eq!(report.sweep.uploads_deleted, 0, "{report:?}");
    assert_eq!(keys(store), all);

    clock.advance(DEFAULT_GRACE + Duration::from_hours(1));
    let report = gc_at(&db, &clock);
    assert_eq!(report.sweep.uploads_deleted, 2, "{report:?}");
    assert_eq!(report.sweep.blobs_deleted, 0, "{report:?}");
    assert_eq!(keys(store), bound, "only the bound blob is left");
    read_back(db.blobs(), &r, &data);
}

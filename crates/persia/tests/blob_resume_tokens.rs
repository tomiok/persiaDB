//! Resume tokens through `put_blob` (SPEC §9.4, §9.5, §9.6; ROADMAP 9.3.4, 9.3.6; ADR-0053), over the remote layout on
//! a clocked `MemoryStorage`, whose keys are listed exactly, and over the pack.
//!
//! Invariants:
//! 1. A remote upload whose reader failed after a window was staged, resumed with the token and the whole blob, binds
//!    the `BlobRef` an uninterrupted `put_blob` binds under the token's upload; no `put_if_absent` reaches a kept
//!    chunk (each chunk after the run, and the manifest, gets exactly one), and `blobs/` holds exactly the manifest
//!    and the upload's chunks.
//! 2. A resume on the pack is `ResumeUnsupported`: nothing is read from the reader, no pin remains, and the pack
//!    file is unchanged (absent before the first blob, the same bytes after it).
//! 3. A token holds only an upload id and a chunk size, so an unknown upload id and a token of another prefix or
//!    bucket are one case. An empty listing starts a fresh upload under the token's id (a blob of at most 64 KiB is
//!    stored chunked, not inline). Chunks under that id holding other bytes at an index the resume writes, with none
//!    it keeps, conflict: the upload moves to a new id and leaves them as they were. Full chunks of the token's size
//!    that are not the reader's bytes are `InvalidResume`, and nothing is staged.
//! 4. A token chunk size below the floor fails before anything is read or staged, and leaves no pin.
//! 5. A token whose chunk size is not the one the upload was staged with keeps no chunk: its chunk 0 is not of the
//!    token's length unless the blob is that one short chunk (which is then kept, invariant 7). The first staged
//!    index the resume writes conflicts (chunk 0 here; it may have been swept), and the reader's blob binds at the
//!    token's chunk size under a new upload id; GC past the grace period then takes the old chunks.
//! 6. After failed resumes (one that stages nothing, one that fails after a conflict restage, and a replayed stale
//!    token that restages again) and the resume that binds, a GC within the grace period deletes nothing, and one
//!    past it leaves exactly the bound blob's manifest and its chunks under `blobs/`.
//! 7. Pinned edges: a token of another store binds under its id there and still resumes at home; an unknown id with
//!    an empty blob binds it chunked under a new id (nothing new to write, ROADMAP 9.3.6); every kept chunk is
//!    checked against the reader (`InvalidResume`, which leaves the token usable), and a reader ending where the kept
//!    run ends binds nothing: a resume carries the whole blob's `expected_size` or `expected_id`, which tells that
//!    prefix from the blob, and one with neither is `ResumeUnchecked` before anything is read (ROADMAP 9.3.7); a
//!    one-chunk upload keeps its short chunk for a token of that length; a token below the floor on the pack is
//!    `ChunkSizeBelowMin`, not `ResumeUnsupported`; a stale token replayed after GC restages under its id, which the
//!    next GC takes.
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

use core::num::{NonZeroU64, NonZeroUsize};
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
use persia_storage::faulty::{Fault, FaultyStorage, RandomFaults};
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::{KeyPrefix, MemoryStorage, Op, Storage};

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

/// A resume of `token` checked by the whole blob's size (ROADMAP 9.3.7: a resume needs that or `expected_id`).
fn resume(token: ResumeToken, blob: &[u8]) -> PutBlobOptions {
    PutBlobOptions {
        resume: Some(token),
        expected_size: Some(blob.len() as u64),
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
fn keys<S: Storage>(store: &RemoteBlobStore<S>) -> BTreeSet<String> {
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
fn bound_keys<S: Storage>(store: &RemoteBlobStore<S>, r: &BlobRef) -> (UploadId, BTreeSet<String>) {
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

/// The `put_if_absent` calls `storage` received since `from` (an index into its log), as keys in call order. Every
/// call is logged: the only fault armed is a zero `Latency`, which changes nothing.
fn put_if_absent_keys(storage: &FaultyStorage<MemoryStorage>, from: usize) -> Vec<String> {
    storage.injected()[from..]
        .iter()
        .filter(|i| i.op == Op::PutIfAbsent)
        .map(|i| i.target.clone())
        .collect()
}

#[test]
fn an_interrupted_remote_upload_resumes_through_put_blob_without_rewriting_its_kept_chunks() {
    // A wrapper that logs every call with its key: `MemoryStorage` answers a same-bytes `put_if_absent` `Created`
    // without touching the object, so only a count of calls shows the kept run is never written again.
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_hours(1_000)));
    let memory = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    let storage = FaultyStorage::new(memory, 1).with_clock(ManualClock::new(UNIX_EPOCH));
    storage.set_random(Some(RandomFaults {
        one_in: NonZeroU64::MIN,
        faults: vec![Fault::Latency(Duration::ZERO)],
    }));
    let store = Arc::new(RemoteBlobStore::new(storage).with_parallelism(NonZeroUsize::MIN));
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(1))
        .with_retry_clock(Arc::new(ManualRetryClock::new()));
    let data = bytes((WINDOW + 5) * CHUNK_LEN + 100, 1);
    let (token, source) = interruption(
        blobs
            .put_blob(fail_in(&data, WINDOW + 2), &options(CHUNK))
            .unwrap_err(),
    );
    assert!(matches!(source, Error::Read(_)), "{source:?}");
    assert_eq!(token.chunk_size, size(CHUNK));
    let kept = chunk_keys(token.upload_id, 0..WINDOW as u32);
    assert_eq!(
        keys(&store),
        kept,
        "the first window, staged before the reader failed"
    );
    assert_eq!(
        put_if_absent_keys(store.storage(), 0).len(),
        WINDOW,
        "one write per staged chunk"
    );
    assert!(blobs.pin_set().pinned().is_empty());

    let before = store.storage().injected().len();
    let (r, pins) = blobs.put_blob(&data[..], &resume(token, &data)).unwrap();
    assert_eq!(r, uninterrupted(&data, CHUNK));
    let (upload, expected) = bound_keys(&store, &r);
    assert_eq!(upload, token.upload_id, "bound under the token's upload");
    assert_eq!(expected.len(), WINDOW + 6 + 1);
    assert_eq!(keys(&store), expected);
    // The kept run was read from the caller and checked, not written again: one `put_if_absent` for each chunk after
    // it, then one for the manifest.
    let mut written: Vec<String> = chunk_keys(token.upload_id, WINDOW as u32..WINDOW as u32 + 6)
        .into_iter()
        .collect();
    written.push(manifest_key(r.blob_id()));
    let calls = put_if_absent_keys(store.storage(), before);
    assert_eq!(calls, written);
    assert!(
        calls.iter().all(|key| !kept.contains(key)),
        "a kept chunk was written"
    );
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
        let e = blobs.put_blob(&mut reader, &resume(t, &data)).unwrap_err();
        assert!(matches!(e, Error::ResumeUnsupported), "{e:?}");
        assert_eq!(reader.reads, 0, "the reader was read");
        // `put_blob` pins the token's upload before asking the store, and drops the pin with the error.
        assert!(blobs.pin_set().pinned().is_empty(), "a pin remains");
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
        let (r, _pins) = blobs.put_blob(&data[..], &resume(t, &data)).unwrap();
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
        let (r, _pins) = blobs.put_blob(&data[..], &resume(t, &data)).unwrap();
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
        ..resume(t, &data)
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
    let (r, _pins) = blobs.put_blob(&data[..], &resume(t, &data)).unwrap();
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
    let e = blobs.put_blob(&data[..], &resume(t, &data)).unwrap_err();
    assert!(matches!(e, Error::InvalidResume { .. }), "{e:?}");
    assert_eq!(keys(store), before, "nothing is staged");
    assert!(blobs.pin_set().pinned().is_empty());
    // A new upload stores the blob.
    let (r, _pins) = blobs.put_blob(&data[..], &options(CHUNK)).unwrap();
    assert_eq!(r, uninterrupted(&data, CHUNK));
}

// -- 4. a token chunk size below the floor --

#[test]
fn a_token_chunk_size_below_the_floor_reads_and_stages_nothing_and_leaves_no_pin() {
    // The floor is checked before `put_blob` pins anything or calls the store, so no call of either could observe a
    // pin: what is checked is that none remains.
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
        let e = blobs.put_blob(&mut reader, &resume(t, &data)).unwrap_err();
        assert!(
            matches!(e, Error::ChunkSizeBelowMin { chunk_size, min: CHUNK } if chunk_size == below),
            "{e:?}"
        );
        assert_eq!(reader.reads, 0, "{below}: the reader was read");
        assert_eq!(keys(store), before, "{below}");
        assert!(
            blobs.pin_set().pinned().is_empty(),
            "{below}: a pin remains"
        );
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
        let blob = Blob::from_reader(&data[..]).with_options(resume(t, &data));
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
                .put_blob(fail_in(&data, WINDOW + 10), &resume(t0, &data))
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
                .put_blob(fail_in(&data, 2 * WINDOW + 2), &resume(t0, &data))
                .unwrap_err(),
        );
        assert_ne!(t1.upload_id, t0.upload_id);
        assert!(matches!(source, Error::Read(_)), "{source:?}");
        // The stale token, replayed: it conflicts and restages again, to a third upload, and fails the same way.
        let (t2, _) = interruption(
            blobs
                .put_blob(fail_in(&data, 2 * WINDOW + 2), &resume(t0, &data))
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
    let blob = Blob::from_reader(&data[..]).with_options(resume(t1, &data));
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

// -- 7. tightened cases (the audit's follow-up) --

/// `InvalidResume`'s `what`, or a panic naming `e`.
fn invalid_resume(e: &Error) -> &'static str {
    match e {
        Error::InvalidResume { what } => what,
        other => panic!("not InvalidResume: {other:?}"),
    }
}

#[test]
fn a_token_of_another_store_stages_the_whole_blob_under_its_id_and_still_resumes_at_home() {
    // Invariant 3: a token holds no bucket or prefix, so another store's token lists empty here.
    let data = bytes((WINDOW + 3) * CHUNK_LEN + 100, 14);
    let (_, home) = remote(14);
    let (t, _) = interruption(
        home.put_blob(fail_in(&data, WINDOW + 1), &options(CHUNK))
            .unwrap_err(),
    );
    let home_keys = keys(home.store());
    assert_eq!(home_keys, chunk_keys(t.upload_id, 0..WINDOW as u32));

    let (_, other) = remote(15);
    let (r, pins) = other.put_blob(&data[..], &resume(t, &data)).unwrap();
    assert_eq!(r, uninterrupted(&data, CHUNK));
    let (upload, expected) = bound_keys(other.store(), &r);
    assert_eq!(upload, t.upload_id, "under the token's id, not a new one");
    assert_eq!(keys(other.store()), expected);
    read_back(&other, &r, &data);
    drop(pins);
    assert!(other.pin_set().pinned().is_empty());
    assert_eq!(keys(home.store()), home_keys, "the token's own store");

    // At home the token keeps its window and binds the same reference under the same id.
    let (again, _pins) = home.put_blob(&data[..], &resume(t, &data)).unwrap();
    assert_eq!(again, r);
    let (upload, expected) = bound_keys(home.store(), &again);
    assert_eq!(upload, t.upload_id);
    assert_eq!(keys(home.store()), expected);
}

#[test]
fn an_unknown_upload_id_and_an_empty_blob_binds_the_empty_blob_chunked_under_a_new_id() {
    // The edge of invariant 3: no chunk to stage at all. The reference is the one a new upload binds, but stored
    // chunked (a resume never takes the small path), with no chunk. The resume writes no chunk the listing did not
    // show (vacuously), so ROADMAP 9.3.6 moves it to a new upload id, not the token's: the one case where an empty
    // listing does not bind under the token's id.
    let (_, blobs) = remote(16);
    let t = token(0xe0e0, CHUNK);
    let (r, pins) = blobs.put_blob(&[][..], &resume(t, &[])).unwrap();
    assert_eq!(r, uninterrupted(&[], CHUNK));
    assert_eq!(r.size(), 0);
    let m = blobs.store().manifest(r.blob_id()).unwrap().unwrap();
    assert!(!m.is_inline());
    assert_eq!(m.chunk_count(), 0);
    assert_ne!(m.upload_id(), t.upload_id);
    assert_eq!(
        keys(blobs.store()),
        BTreeSet::from([manifest_key(r.blob_id())]),
        "only the manifest"
    );
    read_back(&blobs, &r, &[]);
    drop(pins);
    assert!(blobs.pin_set().pinned().is_empty());
}

#[test]
fn a_kept_run_the_reader_does_not_match_is_invalid_resume_whichever_kept_chunk_differs() {
    // Invariant 3: full chunks of the token's size are the kept run, and every one is checked, not only the first.
    let data = bytes(5 * CHUNK_LEN + 100, 17);
    let t = token(0x0c4e_c4ed, CHUNK);
    let flipped = |at: usize| {
        let mut other = data.clone();
        other[at] ^= 0x80;
        other
    };
    let cases: [(&str, Vec<u8>, &str); 5] = [
        ("chunk 0 differs", flipped(3), "not the ones"),
        ("chunk 1 differs", flipped(CHUNK_LEN + 7), "not the ones"),
        (
            "the last kept chunk's last byte differs",
            flipped(3 * CHUNK_LEN - 1),
            "not the ones",
        ),
        (
            "ends inside the run",
            data[..2 * CHUNK_LEN + 9].to_vec(),
            "ends inside",
        ),
        ("empty", Vec::new(), "ends inside"),
    ];
    for (what, reader, kind) in cases {
        let (_, blobs) = remote(17);
        let store = blobs.store();
        for (index, chunk) in (0..).zip(data.chunks(CHUNK_LEN).take(3)) {
            store.stage_chunk(t.upload_id, index, chunk).unwrap();
        }
        let before = keys(store);
        let e = blobs.put_blob(&reader[..], &resume(t, &data)).unwrap_err();
        assert!(invalid_resume(&e).contains(kind), "{what}: {e:?}");
        assert_eq!(keys(store), before, "{what}: nothing is staged");
        assert!(blobs.pin_set().pinned().is_empty(), "{what}");
        // A caller's error leaves the token as good as it was.
        let (r, _pins) = blobs.put_blob(&data[..], &resume(t, &data)).unwrap();
        assert_eq!(r, uninterrupted(&data, CHUNK), "{what}");
        let (upload, expected) = bound_keys(store, &r);
        assert_eq!(upload, t.upload_id, "{what}");
        assert_eq!(keys(store), expected, "{what}");
    }
}

#[test]
fn a_reader_that_ends_where_the_kept_run_ends_binds_nothing() {
    // ROADMAP 9.3.7. Not inside the run, so not `InvalidResume`: the bytes read are a whole blob, the kept run's
    // prefix. A resume must carry the whole blob's size or id, and either tells that prefix from the blob: nothing is
    // bound. Every chunk it stages was listed, so the upload had moved to a young one (ROADMAP 9.3.6), which the
    // mismatch discards; the token's chunks stay as they were. Without either check the resume is refused before
    // anything is read, so the silent short bind cannot happen.
    let data = bytes(5 * CHUNK_LEN, 18);
    let prefix = &data[..3 * CHUNK_LEN];
    let whole_id = persia_blob::blob_id_of(&data);
    let cases = [
        ("expected_size", Some(data.len() as u64), None),
        ("expected_id", None, Some(whole_id)),
        ("neither", None, None),
    ];
    for (what, expected_size, expected_id) in cases {
        let (_, blobs) = remote(18);
        let store = blobs.store();
        let t = token(0x00e2_d5a7, CHUNK);
        for (index, chunk) in (0..).zip(data.chunks(CHUNK_LEN).take(3)) {
            store.stage_chunk(t.upload_id, index, chunk).unwrap();
        }
        let theirs = keys(store);
        let options = PutBlobOptions {
            expected_size,
            expected_id,
            ..resume(t, &data)
        };
        let mut reader = Counting {
            data: prefix,
            reads: 0,
        };
        let e = blobs.put_blob(&mut reader, &options).unwrap_err();
        match what {
            "expected_size" => assert!(
                matches!(e, Error::BlobSizeMismatch { expected, actual }
                    if expected == data.len() as u64 && actual == prefix.len() as u64),
                "{what}: {e:?}"
            ),
            "expected_id" => assert!(
                matches!(e, Error::BlobIdMismatch { expected, .. } if expected == whole_id),
                "{what}: {e:?}"
            ),
            _ => {
                assert!(matches!(e, Error::ResumeUnchecked), "{what}: {e:?}");
                assert_eq!(reader.reads, 0, "{what}: the reader was read");
            }
        }
        assert!(!e.is_corruption(), "{what}");
        assert_eq!(keys(store), theirs, "{what}: nothing bound or left staged");
        assert!(blobs.pin_set().pinned().is_empty(), "{what}");
        // The whole blob still resumes from the token.
        let (r, _pins) = blobs.put_blob(&data[..], &resume(t, &data)).unwrap();
        assert_eq!(r, uninterrupted(&data, CHUNK), "{what}");
        read_back(&blobs, &r, &data);
    }
}

#[test]
fn a_resume_without_an_expected_id_or_size_is_refused_before_anything_is_read() {
    // ROADMAP 9.3.7: on the remote layout and on the pack (an options check: before the store is asked to resume,
    // so before `ResumeUnsupported`). Nothing is read, staged or created, and no pin remains.
    let data = bytes(3 * CHUNK_LEN + 100, 22);
    let unchecked = |t: ResumeToken| PutBlobOptions {
        expected_size: None,
        ..resume(t, &data)
    };
    let (_, blobs) = remote(22);
    let t = token(0x0bad_c0de, CHUNK);
    for (index, chunk) in (0..).zip(data.chunks(CHUNK_LEN).take(2)) {
        blobs
            .store()
            .stage_chunk(t.upload_id, index, chunk)
            .unwrap();
    }
    let before = keys(blobs.store());
    let mut reader = Counting {
        data: &data,
        reads: 0,
    };
    let e = blobs.put_blob(&mut reader, &unchecked(t)).unwrap_err();
    assert!(matches!(e, Error::ResumeUnchecked), "{e:?}");
    assert_eq!(reader.reads, 0);
    assert_eq!(keys(blobs.store()), before);
    assert!(blobs.pin_set().pinned().is_empty());

    let dir = tempfile::tempdir().unwrap();
    let db =
        BlobDatabase::create(dir.path().join("app.persia"), DatabaseOptions::default()).unwrap();
    let mut reader = Counting {
        data: &data,
        reads: 0,
    };
    let e = db.blobs().put_blob(&mut reader, &unchecked(t)).unwrap_err();
    assert!(matches!(e, Error::ResumeUnchecked), "{e:?}");
    assert_eq!(reader.reads, 0);
    assert!(!dir.path().join("app.persia.blobs").exists());
    assert!(db.blobs().pin_set().pinned().is_empty());
}

#[test]
fn a_one_chunk_upload_resumed_at_its_short_chunks_length_keeps_that_chunk() {
    // The exception of invariant 5: an upload staged at 4 × the floor of a blob of 2 × the floor is one short
    // chunk 0, which is a full chunk of a token at 2 × the floor. The token at the upload's own size keeps nothing,
    // and its chunk 0 is the same bytes, which is no conflict either.
    let data = bytes(2 * CHUNK_LEN, 19);
    let other = bytes(2 * CHUNK_LEN, 20);
    let old = UploadId::new(0x0001_c4a2);
    for (what, staged, token_at) in [
        ("kept at the short chunk's length", &data, 2 * CHUNK),
        ("the same bytes at the upload's size", &data, 4 * CHUNK),
    ] {
        let (_, blobs) = remote(19);
        let store = blobs.store();
        store.stage_chunk(old, 0, staged).unwrap();
        let theirs = keys(store);
        let t = ResumeToken {
            upload_id: old,
            chunk_size: size(token_at),
        };
        let (r, _pins) = blobs.put_blob(&data[..], &resume(t, &data)).unwrap();
        assert_eq!(r, uninterrupted(&data, token_at), "{what}");
        assert_eq!(r.chunk_size(), token_at, "{what}");
        let (upload, bound) = bound_keys(store, &r);
        // Nothing new to write: the chunk moves to a young upload (ROADMAP 9.3.6).
        assert_ne!(upload, old, "{what}");
        assert_eq!(
            keys(store),
            bound.union(&theirs).cloned().collect(),
            "{what}"
        );
        read_back(&blobs, &r, &data);
    }
    // Other bytes in that chunk, at its length: a kept chunk the reader does not match.
    let (_, blobs) = remote(20);
    let store = blobs.store();
    store.stage_chunk(old, 0, &other).unwrap();
    let before = keys(store);
    let t = ResumeToken {
        upload_id: old,
        chunk_size: size(2 * CHUNK),
    };
    let e = blobs.put_blob(&data[..], &resume(t, &data)).unwrap_err();
    assert!(invalid_resume(&e).contains("not the ones"), "{e:?}");
    assert_eq!(keys(store), before);
    assert!(blobs.pin_set().pinned().is_empty());
}

#[test]
fn a_token_chunk_size_below_the_floor_on_the_pack_is_below_the_floor_not_unsupported() {
    // Invariant 4 comes before invariant 2: the chunk size is checked before the store is asked to resume.
    let dir = tempfile::tempdir().unwrap();
    let db =
        BlobDatabase::create(dir.path().join("app.persia"), DatabaseOptions::default()).unwrap();
    let pack = dir.path().join("app.persia.blobs");
    let blobs = db.blobs();
    let data = bytes(2 * CHUNK_LEN, 21);
    let t = ResumeToken {
        upload_id: UploadId::new(7),
        chunk_size: ChunkSize::from_encoding(CHUNK - 1).unwrap(),
    };
    let mut reader = Counting {
        data: &data,
        reads: 0,
    };
    let e = blobs.put_blob(&mut reader, &resume(t, &data)).unwrap_err();
    assert!(
        matches!(e, Error::ChunkSizeBelowMin { chunk_size, min: CHUNK } if chunk_size == CHUNK - 1),
        "{e:?}"
    );
    assert_eq!(reader.reads, 0);
    assert!(!pack.exists());
    assert!(blobs.pin_set().pinned().is_empty(), "a pin remains");
}

#[test]
fn stale_tokens_replayed_after_gc_restage_and_the_next_gc_takes_them() {
    // Invariant 6 continued: a token whose upload GC swept lists empty, so it stages the whole blob under its id
    // again; the blob is bound already and a resume never discards the token's upload (ADR-0053 item 11), so the
    // next GC past the grace period takes those chunks. The bound upload's own token restages (ROADMAP 9.3.6) and
    // discards its duplicate at once.
    let dir = tempfile::tempdir().unwrap();
    let (clock, mut db) = database(dir.path(), 22);
    let data = bytes((WINDOW + 3) * CHUNK_LEN + 100, 22);
    let (t0, _) = interruption(
        db.blobs()
            .put_blob(fail_in(&data, WINDOW + 1), &options(CHUNK))
            .unwrap_err(),
    );
    // Other bytes at chunk `WINDOW + 1`, so the resume restages to a new upload, which it binds.
    db.blobs()
        .store()
        .stage_chunk(t0.upload_id, WINDOW as u32 + 1, &bytes(100, 23))
        .unwrap();
    let blob = Blob::from_reader(&data[..]).with_options(resume(t0, &data));
    let r = db
        .put("c", &Document::new("d"), blob, Durability::Fsync)
        .unwrap();
    db.release_durable();
    let store = db.blobs().store();
    let (t1, bound) = bound_keys(store, &r);
    assert_ne!(t1, t0.upload_id);
    let chunks = store.manifest(r.blob_id()).unwrap().unwrap().chunk_count();
    clock.advance(DEFAULT_GRACE + Duration::from_hours(1));
    let report = gc_at(&db, &clock);
    assert_eq!(report.sweep.uploads_deleted, 1, "{report:?}");
    assert_eq!(keys(store), bound);

    let (again, pins) = db.blobs().put_blob(&data[..], &resume(t0, &data)).unwrap();
    assert_eq!(again, r, "a duplicate of the bound blob");
    drop(pins);
    let mut expected = bound.clone();
    expected.extend(chunk_keys(t0.upload_id, 0..chunks));
    assert_eq!(
        keys(store),
        expected,
        "the swept token's upload, staged again"
    );

    let own = ResumeToken {
        upload_id: t1,
        chunk_size: size(CHUNK),
    };
    let (again, pins) = db.blobs().put_blob(&data[..], &resume(own, &data)).unwrap();
    assert_eq!(again, r);
    drop(pins);
    assert_eq!(keys(store), expected, "the restaged duplicate is discarded");
    assert!(db.blobs().pin_set().pinned().is_empty());

    let report = gc_at(&db, &clock);
    assert_eq!(report.sweep.uploads_deleted, 0, "young: {report:?}");
    assert_eq!(keys(store), expected);
    clock.advance(DEFAULT_GRACE + Duration::from_hours(1));
    let report = gc_at(&db, &clock);
    assert_eq!(report.sweep.uploads_deleted, 1, "{report:?}");
    assert_eq!(report.sweep.blobs_deleted, 0, "{report:?}");
    assert_eq!(keys(store), bound, "only the bound blob is left");
    read_back(db.blobs(), &r, &data);
}

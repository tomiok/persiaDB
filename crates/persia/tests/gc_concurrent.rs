//! GC concurrent with `put_blob` and `get_blob` on other threads, over the writer's real `PinSet` (SPEC §9.5,
//! §18.10 (c); ROADMAP 9.5.9; ADR-0051, ADR-0052), on both layouts: the remote one on a clocked `MemoryStorage`, whose
//! clock GC moves past the grace period before every round, and the local pack opened as the container's writer, which
//! every round rewrites.
//!
//! Invariants:
//! 1. No reader sees an error for a blob its pins or a document keep: every `put_blob` reads back, streamed and by
//!    range, while its pins are held, and every blob a document references reads back throughout.
//! 2. Nothing referenced is swept: no GC round reports a referenced blob missing, and every kept blob reads back after
//!    the last round.
//! 3. Garbage is eventually collected: once the pins are dropped, the next rounds delete every orphan, and remotely
//!    leave no chunk outside the uploads the kept blobs' manifests name.
//!
//! Deterministic inputs: the blobs' bytes come from fixed seeds and every thread runs a fixed number of iterations;
//! only the interleaving varies, and the invariants hold for every one. No sleeps: barriers start the threads
//! together and run a GC round between each `put_blob` and the drop of its pins, while its writer reads the blob.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::manual_assert_eq,
    clippy::assert_is_empty
)] // Tests: inputs are fixed; `assert!(a == b)` keeps megabytes of blob bytes out of a failure message.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::{Blob, BlobDatabase};
use persia::internal::gc::{DEFAULT_GRACE, GcOptions};
use persia::internal::pins::Pins;
use persia_blob::gc::Sweep;
use persia_blob::remote::{RemoteBlobStore, RemoteKey, keys};
use persia_blob::{BlobRef, BlobStore, ChunkSize, PackStore, UploadId};
use persia_engine::db::{Database, DatabaseOptions, SegmentIds};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::{MemoryStorage, Storage};

const CHUNK: usize = ChunkSize::MIN as usize;

/// How much each run does.
#[derive(Debug, Clone, Copy)]
struct Load {
    writers: usize,
    iterations: usize,
    readers: usize,
    /// GC rounds at least, however fast the writers are.
    rounds: usize,
    /// Blobs a document references, put before the threads start.
    documents: usize,
}

const FAST: Load = Load {
    writers: 2,
    iterations: 6,
    readers: 1,
    rounds: 4,
    documents: 2,
};

#[cfg(feature = "slow")]
const SLOW: Load = Load {
    writers: 4,
    iterations: 40,
    readers: 2,
    rounds: 24,
    documents: 6,
};

/// The bytes of blob `i` of `thread`: unique (they start with both), of a size by `i`: a small blob (stored in one
/// step), one of two chunks, or one of three and a bit.
fn data(thread: usize, i: usize) -> Vec<u8> {
    let len = match i % 3 {
        0 => 300 + i,
        1 => CHUNK + 17 * i + 1,
        _ => 3 * CHUNK + 5,
    };
    let mut state = (thread as u64) << 32 | i as u64;
    let mut out: Vec<u8> = Vec::with_capacity(len);
    out.extend_from_slice(&(thread as u32).to_le_bytes());
    out.extend_from_slice(&(i as u32).to_le_bytes());
    while out.len() < len {
        // SplitMix64.
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out.push((z ^ (z >> 31)) as u8);
    }
    out
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(ChunkSize::MIN).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn log(dir: &Path) -> Database {
    let mut log = Database::create(dir.join("app.persia"), DatabaseOptions::default()).unwrap();
    log.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    log
}

fn read_all<S: BlobStore + ?Sized>(db: &BlobDatabase<S>, r: &BlobRef) -> Vec<u8> {
    let mut stream = db.get_blob(r).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    out
}

/// Reads `r` streamed and by range, and records a failure unless both are `bytes`.
fn check<S: BlobStore + ?Sized>(
    db: &BlobDatabase<S>,
    r: &BlobRef,
    bytes: &[u8],
    what: &str,
    fail: &(dyn Fn(String) + Sync),
) {
    let streamed = db
        .get_blob(r)
        .map_err(|e| format!("{e:?}"))
        .and_then(|mut s| {
            let mut out = Vec::new();
            s.read_to_end(&mut out)
                .map(|_| out)
                .map_err(|e| format!("{e:?}"))
        });
    match streamed {
        Ok(out) if out == bytes => {}
        Ok(_) => fail(format!("{what}: streamed other bytes")),
        Err(e) => fail(format!("{what}: stream: {e}")),
    }
    let mid = r.size() / 2;
    match db.get_blob_range(r, mid..r.size()) {
        Ok(out) if out == bytes[mid as usize..] => {}
        Ok(_) => fail(format!("{what}: another range")),
        Err(e) => fail(format!("{what}: range: {e:?}")),
    }
}

/// What a run leaves: the blobs documents reference, with their bytes, and the orphans (every `put_blob` result,
/// whose pins are all dropped by the end).
struct Outcome {
    kept: Vec<(BlobRef, Vec<u8>)>,
    orphans: Vec<BlobRef>,
}

/// Puts `load.documents` documents with blobs, then runs writers, readers and GC at once; `before_round` runs before
/// each GC round (the remote layout moves its clock there). Checks invariants 1 and 2, then drops every pin and runs
/// GC until a round deletes nothing.
fn run<S: BlobStore + Sweep + ?Sized>(
    db: &mut BlobDatabase<S>,
    load: Load,
    gc: &(dyn Fn() -> GcOptions + Sync),
    before_round: &(dyn Fn() + Sync),
) -> Outcome {
    let mut kept = Vec::new();
    for k in 0..load.documents {
        let bytes = data(usize::MAX, k);
        let r = db
            .put(
                "c",
                &Document::new(format!("d{k}")),
                Blob::from_reader(&bytes[..]).with_options(options()),
                Durability::Fsync,
            )
            .unwrap();
        kept.push((r, bytes));
    }
    db.release_durable();

    let db = &*db;
    // Lock step: after each `put_blob`, every writer and GC meet at `step`; GC runs a round while the writers read
    // their new blobs (pins held), and they meet again before the writers read once more, so every blob is read both
    // across and after a round its pins alone keep it through. Failures are collected, not panicked on, so a failing
    // thread never leaves the others waiting at the barrier.
    let start = Barrier::new(load.writers + load.readers + 1);
    let step = Barrier::new(load.writers + 1);
    let finished = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let fail = |what: String| failures.lock().unwrap().push(what);
    let (held, mut orphans) = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..load.writers)
            .map(|t| {
                let (start, step, finished, fail) = (&start, &step, &finished, &fail);
                scope.spawn(move || {
                    start.wait();
                    let mut held: Vec<(BlobRef, Vec<u8>, Pins)> = Vec::new();
                    let mut orphans = Vec::new();
                    for i in 0..load.iterations {
                        let bytes = data(t, i);
                        let put = db.put_blob(&bytes[..], &options());
                        step.wait();
                        let (r, pins) = match put {
                            Ok(put) => put,
                            Err(e) => {
                                fail(format!("put_blob {t}/{i}: {e:?}"));
                                step.wait();
                                continue;
                            }
                        };
                        // Invariant 1: its pins keep it while GC runs, and after.
                        check(
                            db,
                            &r,
                            &bytes,
                            &format!("blob {t}/{i} during a round"),
                            fail,
                        );
                        step.wait();
                        check(db, &r, &bytes, &format!("blob {t}/{i} after a round"), fail);
                        orphans.push(r.clone());
                        if i % 3 == 2 {
                            held.push((r, bytes, pins));
                        }
                    }
                    finished.fetch_add(1, Ordering::AcqRel);
                    (held, orphans)
                })
            })
            .collect();
        for _ in 0..load.readers {
            let (start, finished, kept, fail) = (&start, &finished, &kept, &fail);
            scope.spawn(move || {
                start.wait();
                // Bounded: the writers finish.
                while finished.load(Ordering::Acquire) < load.writers {
                    for (r, bytes) in kept {
                        check(db, r, bytes, "a referenced blob", fail);
                    }
                }
            });
        }
        start.wait();
        let round = |rounds: &mut usize| {
            before_round();
            match db.gc(&gc()) {
                // Invariant 2: no referenced blob is missing.
                Ok(report) if report.missing.is_empty() => {}
                other => fail(format!("GC round {rounds}: {other:?}")),
            }
            *rounds += 1;
        };
        let mut rounds = 0;
        for _ in 0..load.iterations {
            step.wait();
            round(&mut rounds);
            step.wait();
        }
        while rounds < load.rounds {
            round(&mut rounds);
        }
        let mut held = Vec::new();
        let mut orphans = Vec::new();
        for writer in writers {
            let (h, o) = writer.join().unwrap();
            held.extend(h);
            orphans.extend(o);
        }
        (held, orphans)
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{failures:#?}");

    // Invariant 2: after the last round, every kept blob is whole.
    for (r, bytes, _) in &held {
        assert!(read_all(db, r) == *bytes);
    }
    for (r, bytes) in &kept {
        assert!(read_all(db, r) == *bytes);
    }
    // Invariant 3: with the pins dropped, GC collects every orphan.
    drop(held);
    for _ in 0..3 {
        before_round();
        db.gc(&gc()).unwrap();
    }
    before_round();
    let report = db.gc(&gc()).unwrap();
    assert_eq!(report.sweep.blobs_deleted, 0, "{report:?}");
    assert_eq!(report.sweep.uploads_deleted, 0, "{report:?}");
    for r in &orphans {
        assert!(
            !db.blobs().store().contains(r).unwrap(),
            "an orphan was kept"
        );
    }
    for (r, bytes) in &kept {
        assert!(read_all(db, r) == *bytes);
    }
    orphans.retain(|r| kept.iter().all(|(k, _)| k != r));
    Outcome { kept, orphans }
}

fn remote(load: Load) {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_hours(1_000)));
    let storage = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    let store = Arc::new(RemoteBlobStore::new(storage));
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(21));
    let mut db = BlobDatabase::new(log(dir.path()), blobs);
    let gc = || GcOptions {
        now: Some(clock.now()),
        ..GcOptions::default()
    };
    // Every round, whatever was written before it is older than the grace period: only marks and pins keep it.
    let before_round = || clock.advance(DEFAULT_GRACE + Duration::from_hours(1));
    let outcome = run(&mut db, load, &gc, &before_round);

    // Invariant 3, remotely: no chunk is left outside the uploads the kept blobs' manifests name.
    let named: HashSet<UploadId> = outcome
        .kept
        .iter()
        .filter_map(|(r, _)| {
            let m = store.manifest(r.blob_id()).unwrap().unwrap();
            (!m.is_inline()).then(|| m.upload_id())
        })
        .collect();
    let left: HashSet<UploadId> = store
        .storage()
        .list(&keys::uploads_prefix().unwrap())
        .unwrap()
        .iter()
        .filter_map(|meta| match RemoteKey::parse(&meta.key)? {
            RemoteKey::Chunk { upload_id, .. } => Some(upload_id),
            RemoteKey::Manifest(_) => None,
        })
        .collect();
    assert_eq!(left, named);
    assert!(!outcome.orphans.is_empty());
}

fn pack(load: Load) {
    let dir = tempfile::tempdir().unwrap();
    let log = log(dir.path());
    // ROADMAP 9.2.7: the pack opens read-write only as the container's writer.
    let proof = log.writer_proof().unwrap();
    let pack = PackStore::open_writer(dir.path().join("app.persia.blobs"), &proof).unwrap();
    let blobs = Blobs::with_upload_ids(Arc::new(pack), SegmentIds::seeded(22));
    let mut db = BlobDatabase::new(log, blobs);
    // Every round rewrites the pack, so streams and range reads cross rewrites (SPEC §18.10 (c)).
    let gc = || GcOptions {
        rewrite_ratio: 0.0,
        ..GcOptions::default()
    };
    let outcome = run(&mut db, load, &gc, &|| {});
    assert!(!outcome.orphans.is_empty());
}

#[test]
fn gc_runs_beside_put_blob_and_get_blob_on_the_remote_layout() {
    remote(FAST);
}

#[test]
fn gc_runs_beside_put_blob_and_get_blob_on_the_pack() {
    pack(FAST);
}

#[cfg(feature = "slow")]
#[test]
fn gc_runs_beside_many_put_blobs_and_get_blobs_on_the_remote_layout() {
    remote(SLOW);
}

#[cfg(feature = "slow")]
#[test]
fn gc_runs_beside_many_put_blobs_and_get_blobs_on_the_pack() {
    pack(SLOW);
}

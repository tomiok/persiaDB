//! GC and the writer's real pin set (SPEC §9.5, §18.10 (c); ROADMAP 9.5.2, 9.5.4, 9.5.8; ADR-0052), with the pack.
//!
//! Invariants:
//! 1. A pin the writer's `PinSet` takes after the mark (a `get_blob` stream opened then) keeps its blob through the
//!    sweep; one taken after the rewrite's plan, at its pin check, defers the rewrite and leaves the pack unchanged.
//! 2. A `get_blob` stream open across a rewrite reads on, byte for byte, from the new file; once it is dropped the
//!    next GC collects the blob.
#![allow(clippy::unwrap_used, clippy::panic, clippy::cast_possible_truncation)] // Tests: inputs are fixed.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::{Blob, BlobDatabase};
use persia::internal::gc::GcOptions;
use persia::internal::pins::{PinSet, Pinned, Pins};
use persia_blob::gc::{GcObject, PinCheck, PinGate, Rewrite, Sweep, SweepOptions, SweepReport};
use persia_blob::{BlobRef, BlobStore, ChunkSize, PackStore};
use persia_engine::db::{Database, DatabaseOptions, SegmentIds};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed) ^ ((i >> 11) as u8))
        .collect()
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(ChunkSize::MIN).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn pack_database(dir: &Path) -> BlobDatabase<PackStore> {
    let mut log = Database::create(dir.join("app.persia"), DatabaseOptions::default()).unwrap();
    log.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    let pack =
        PackStore::open_writer(dir.join("app.persia.blobs"), &log.writer_proof().unwrap()).unwrap();
    BlobDatabase::new(
        log,
        Blobs::with_upload_ids(Arc::new(pack), SegmentIds::seeded(11)),
    )
}

/// An orphan: stored by `put_blob`, its pins dropped at once.
fn orphan(db: &BlobDatabase<PackStore>, len: usize, seed: u8) -> (BlobRef, Vec<u8>) {
    let data = bytes(len, seed);
    let (r, pins) = db.put_blob(&data[..], &options()).unwrap();
    drop(pins);
    (r, data)
}

const FORCE: SweepOptions = SweepOptions {
    grace: persia::internal::gc::DEFAULT_GRACE,
    now: SystemTime::UNIX_EPOCH,
    rewrite_ratio: 0.0,
};

/// The writer's `PinSet`, with `hook` run before the first pin check (outside the set's lock, as a `get_blob` on
/// another thread would run then).
struct Hooked<'a> {
    set: &'a PinSet,
    hook: Mutex<Option<Box<dyn FnOnce() + 'a>>>,
}

impl PinGate for Hooked<'_> {
    fn pinned(&self) -> Vec<GcObject> {
        PinGate::pinned(self.set)
    }

    fn unless_pinned<T>(
        &self,
        object: GcObject,
        delete: impl FnOnce(&dyn PinCheck) -> T,
    ) -> Option<T> {
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        PinGate::unless_pinned(self.set, object, delete)
    }
}

fn sweep(db: &BlobDatabase<PackStore>, pins: &impl PinGate) -> SweepReport {
    let mark = db.mark().unwrap();
    db.blobs().store().sweep(&mark, pins, &FORCE).unwrap()
}

#[test]
fn a_pin_taken_after_the_mark_keeps_its_blob_and_one_at_the_check_defers_the_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = pack_database(dir.path());
    let kept = db
        .put(
            "c",
            &Document::new("a"),
            Blob::from_reader(&bytes(70 << 10, 1)[..]).with_options(options()),
            Durability::Fsync,
        )
        .unwrap();
    let (read_late, read_data) = orphan(&db, 70 << 10, 2);
    let (small_orphan, small_data) = orphan(&db, 100, 3);
    let (garbage, _) = orphan(&db, 80 << 10, 4);

    // The mark sees no pin on the orphans; a stream opened after it (its blob, then its upload) is in the pin set
    // when the rewrite plans.
    let mark = db.mark().unwrap();
    for r in [&read_late, &small_orphan, &garbage] {
        assert!(!mark.blobs.contains(&r.blob_id()));
    }
    let mut stream = db.get_blob(&read_late).unwrap();
    // And a pin taken after the plan, at the rewrite's pin check, on a blob it drops: not the first one it checks (put
    // first, so checked first), so only the view of the locked pin set (`PinView`) can see it.
    let set: &Arc<PinSet> = db.blobs().pin_set();
    let late: Mutex<Option<Pins>> = Mutex::new(None);
    let hooked = Hooked {
        set,
        hook: Mutex::new(Some(Box::new(|| {
            *late.lock().unwrap() = Some(set.pin([Pinned::Blob(garbage.blob_id())]));
        }))),
    };
    let store = Arc::clone(db.blobs().store());
    let before = store.superblock();
    let report = store.sweep(&mark, &hooked, &FORCE).unwrap();
    assert_eq!(report.rewrite, Some(Rewrite::Deferred), "{report:?}");
    assert_eq!(
        store.superblock(),
        before,
        "a deferred rewrite changes nothing"
    );
    for r in [&kept, &read_late, &small_orphan, &garbage] {
        assert!(store.contains(r).unwrap());
    }
    assert_eq!(
        db.get_blob_range(&small_orphan, 0..100).unwrap(),
        small_data
    );

    // The check's pin released, the next sweep runs: the stream's pin (taken after the mark) still keeps its blob.
    drop(late.lock().unwrap().take());
    let report = sweep(&db, &**db.blobs().pin_set());
    assert!(
        matches!(report.rewrite, Some(Rewrite::Done { .. })),
        "{report:?}"
    );
    assert_eq!(report.blobs_deleted, 2, "{report:?}");
    assert!(!store.contains(&small_orphan).unwrap());
    assert!(!store.contains(&garbage).unwrap());
    let mut back = Vec::new();
    stream.read_to_end(&mut back).unwrap();
    assert_eq!(back, read_data);
    drop(stream);
    let report = db
        .gc(&GcOptions {
            rewrite_ratio: 0.0,
            ..GcOptions::default()
        })
        .unwrap();
    assert_eq!(report.sweep.blobs_deleted, 1, "{report:?}");
    assert!(!store.contains(&read_late).unwrap());
    assert_eq!(
        db.get_blob_range(&kept, 0..kept.size()).unwrap(),
        bytes(70 << 10, 1)
    );
}

/// SPEC §18.10 (c): a stream reads every chunk by id, so one open across a rewrite reads on in the new file. The blob
/// spans two windows (64 chunks of 64 KiB each), so the second is read after the rewrite moved every chunk.
#[test]
fn a_stream_open_across_a_rewrite_reads_on_and_its_blob_goes_once_it_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let db = pack_database(dir.path());
    let (garbage, _) = orphan(&db, 300 << 10, 5);
    let (streamed, data) = orphan(&db, (64 * 64 + 3) << 10, 6);
    let mut stream = db.get_blob(&streamed).unwrap();
    let mut head = vec![0; 1000];
    stream.read_exact(&mut head).unwrap();
    assert_eq!(head, data[..1000]);

    let report = db
        .gc(&GcOptions {
            rewrite_ratio: 0.0,
            ..GcOptions::default()
        })
        .unwrap();
    let Some(Rewrite::Done { generation, .. }) = report.sweep.rewrite else {
        panic!("{report:?}");
    };
    assert_eq!(generation, 2);
    assert_eq!(report.sweep.blobs_deleted, 1, "{report:?}");
    let store = Arc::clone(db.blobs().store());
    assert!(!store.contains(&garbage).unwrap());
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert_eq!(rest.len() + head.len(), data.len());
    assert!(
        rest == data[1000..],
        "the stream read other bytes after the rewrite"
    );
    drop(stream);
    let report = db
        .gc(&GcOptions {
            rewrite_ratio: 0.0,
            ..GcOptions::default()
        })
        .unwrap();
    assert_eq!(report.sweep.blobs_deleted, 1, "{report:?}");
    assert!(!store.contains(&streamed).unwrap());
}

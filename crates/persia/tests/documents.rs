//! `put(doc, blob)` (SPEC §9.5, §9.6; ROADMAP 9.4.1, ADR-0051): the blob durable, then the document logged.
//!
//! Invariants:
//! 1. When the document is logged, its blob is bound in the store and pinned, for every durability level and every
//!    kind of blob (reader, path, a stored reference); the document logged carries the reference `put` returns.
//! 2. A crash at any store call of `put(doc, blob)`, or after it, never leaves a logged or committed document whose
//!    blob the store lacks (an orphan blob is fine): reopening the database finds only documents whose blob is bound.
//! 3. A stored reference the store does not hold is `BlobNotStored` and logs nothing.
//! 4. The blob's pins are held until the document's record is durable: released at once at `Fsync`, at the next
//!    commit or `Fsync` operation at `Buffered` and `Async`, and never while the record may be on disk but not durable.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::assert_is_empty
)] // Tests: inputs are fixed.

#[path = "support/store.rs"]
mod store;

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::{Blob, BlobDatabase, DocumentLog, Error};
use persia::internal::pins::Pinned;
use persia_blob::{BlobRef, BlobStore, ChunkSize, MemoryBlobStore, blob_id_of};
use persia_engine::db::{Database, DatabaseError, DatabaseOptions, SearchRequest, SegmentIds};
use persia_engine::document::{Document, Value};
use persia_engine::query::Query;
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::wal::{Durability, WalMark};
use store::{Call, Faults, FaultyStore};

const LEVELS: [Durability; 3] = [Durability::Buffered, Durability::Async, Durability::Fsync];

fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn doc(id: &str) -> Document {
    let body = schema().field("body").unwrap().id();
    Document::new(id).with(body, Value::String(format!("text of {id}")))
}

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
        .collect()
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(ChunkSize::MIN).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn database(path: &Path) -> Database {
    let mut db = Database::create(path, DatabaseOptions::default()).unwrap();
    db.create_collection(schema()).unwrap();
    db
}

/// Every document of `c` (segments and memtable), with its blob reference.
fn documents(db: &Database) -> Vec<Document> {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let plan = SearchPlan::new(
        &Query::MatchAll,
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let request = SearchRequest {
        limit: 1000,
        ..SearchRequest::default()
    };
    let views = c.views(&memtable).unwrap();
    let results = views.search(&plan, &request, Pruning::default()).unwrap();
    results.hits.into_iter().map(|h| h.document).collect()
}

/// A log that checks, at each put, that the document's blob is bound and pinned (invariant 1).
struct Checked {
    db: Database,
    store: Arc<FaultyStore>,
    pins: Arc<persia::internal::pins::PinSet>,
    logged: Vec<BlobRef>,
}

impl DocumentLog for Checked {
    fn put(
        &mut self,
        collection: &str,
        doc: &Document,
        durability: Durability,
    ) -> Result<(), DatabaseError> {
        let blob = doc
            .blob_ref()
            .expect("put(doc, blob) logs a reference")
            .clone();
        assert!(
            self.store.inner.contains(&blob).unwrap(),
            "logged before its blob is bound"
        );
        assert!(
            self.pins.is_pinned(Pinned::Blob(blob.blob_id())),
            "logged with its blob unpinned"
        );
        self.logged.push(blob);
        self.db.put(collection, doc, durability)
    }

    fn wal_mark(&self) -> WalMark {
        self.db.wal_mark()
    }

    fn durable_mark(&self) -> WalMark {
        self.db.durable_mark()
    }
}

/// Invariant 1, for every durability and kind of blob.
#[test]
fn the_blob_is_bound_and_pinned_when_its_document_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let store = FaultyStore::new(Faults::default());
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(3));
    store.watch(blobs.pin_set());
    let log = Checked {
        db: database(&dir.path().join("a.persia")),
        store: Arc::clone(&store),
        pins: Arc::clone(blobs.pin_set()),
        logged: Vec::new(),
    };
    let mut db = BlobDatabase::new(log, blobs);
    let file = dir.path().join("blob.bin");
    std::fs::write(&file, bytes(150_000, 9)).unwrap();
    let mut n = 0_u8;
    for level in LEVELS {
        for len in [0, 10, 200_000] {
            n += 1;
            let data = bytes(len, n);
            let r = db
                .put(
                    "c",
                    &doc(&format!("r{n}")),
                    Blob::from_reader(&data[..]).with_options(options()),
                    level,
                )
                .unwrap();
            assert_eq!(r.blob_id(), blob_id_of(&data));
            let mut back = Vec::new();
            db.get_blob(&r).unwrap().read_to_end(&mut back).unwrap();
            assert_eq!(back, data);
        }
        let p = db
            .put("c", &doc(&format!("p{n}")), Blob::from_path(&file), level)
            .unwrap();
        assert_eq!(
            db.get_blob_range(&p, 0..150_000).unwrap(),
            bytes(150_000, 9)
        );
        let s = db
            .put("c", &doc(&format!("s{n}")), Blob::Stored(p.clone()), level)
            .unwrap();
        assert_eq!(s, p);
    }
    // Each document was checked as it was logged, and holds the reference `put` returned.
    let logged = db.log().logged.clone();
    assert_eq!(logged.len(), 15);
    let found: Vec<_> = documents(&db.log().db)
        .into_iter()
        .map(|d| d.blob_ref().cloned().unwrap())
        .collect();
    for r in &logged {
        assert!(found.contains(r));
    }
    // The document's own reference is replaced by the one stored.
    let mut other = doc("o");
    other.set_blob_ref(Some(logged[0].clone()));
    let r = db
        .put(
            "c",
            &other,
            Blob::from_reader(&b"other"[..]),
            Durability::Fsync,
        )
        .unwrap();
    assert_eq!(r.blob_id(), blob_id_of(b"other"));
}

/// Invariant 3.
#[test]
fn a_reference_the_store_lacks_logs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = BlobDatabase::new(
        database(&dir.path().join("a.persia")),
        Blobs::new(Arc::new(MemoryBlobStore::new())),
    );
    let elsewhere = Blobs::new(Arc::new(MemoryBlobStore::new()));
    let (foreign, _pins) = elsewhere
        .put_blob(&b"bytes of another database"[..], &options())
        .unwrap();
    let before = db.log().wal_mark();
    let error = db
        .put(
            "c",
            &doc("x"),
            Blob::Stored(foreign.clone()),
            Durability::Fsync,
        )
        .unwrap_err();
    assert!(matches!(error, Error::BlobNotStored { blob_id } if blob_id == foreign.blob_id()));
    assert_eq!(db.log().wal_mark(), before);
    assert!(documents(db.log()).is_empty());
    assert!(db.blobs().pin_set().pinned().is_empty());
    // A missing file logs nothing either.
    let error = db
        .put(
            "c",
            &doc("y"),
            Blob::from_path(dir.path().join("missing")),
            Durability::Fsync,
        )
        .unwrap_err();
    assert!(matches!(error, Error::Open { .. }));
    assert!(documents(db.log()).is_empty());
}

/// Invariant 4.
#[test]
fn pins_are_held_until_the_document_is_durable() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = BlobDatabase::new(
        database(&dir.path().join("a.persia")),
        Blobs::new(Arc::new(MemoryBlobStore::new())),
    );
    let pinned = |db: &BlobDatabase<MemoryBlobStore>, r: &BlobRef| {
        db.blobs().pin_set().is_pinned(Pinned::Blob(r.blob_id()))
    };

    let r = db
        .put(
            "c",
            &doc("f"),
            Blob::from_reader(&b"fsync"[..]),
            Durability::Fsync,
        )
        .unwrap();
    assert_eq!(db.held(), 0);
    assert!(!pinned(&db, &r));
    assert!(db.blobs().pin_set().pinned().is_empty());

    for level in [Durability::Buffered, Durability::Async] {
        let a = db
            .put(
                "c",
                &doc("a"),
                Blob::from_reader(std::io::Cursor::new(format!("first {level:?}"))),
                level,
            )
            .unwrap();
        let b = db
            .put(
                "c",
                &doc("b"),
                Blob::from_reader(std::io::Cursor::new(format!("second {level:?}"))),
                level,
            )
            .unwrap();
        assert_eq!(db.held(), 2, "{level:?}");
        assert!(pinned(&db, &a) && pinned(&db, &b));
        // Its upload is pinned too.
        assert_eq!(db.blobs().pin_set().pinned().len(), 4);
        db.commit().unwrap();
        assert_eq!(db.held(), 0);
        assert!(db.blobs().pin_set().pinned().is_empty(), "{level:?}");

        // A later Fsync operation makes the earlier records durable too.
        let c = db
            .put("c", &doc("c"), Blob::from_reader(&b"third"[..]), level)
            .unwrap();
        assert!(pinned(&db, &c));
        db.put("c", &doc("d"), Blob::Stored(c.clone()), Durability::Fsync)
            .unwrap();
        assert_eq!(db.held(), 0);
        assert!(!pinned(&db, &c));
    }
}

/// A put that fails before its record is appended (no such collection) unpins its blob.
#[test]
fn a_rejected_document_unpins_its_blob() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = BlobDatabase::new(
        database(&dir.path().join("a.persia")),
        Blobs::new(Arc::new(MemoryBlobStore::new())),
    );
    let error = db
        .put(
            "nope",
            &doc("x"),
            Blob::from_reader(&b"orphan"[..]),
            Durability::Buffered,
        )
        .unwrap_err();
    assert!(matches!(error, Error::Database(_)));
    assert_eq!(db.held(), 0);
    assert!(db.blobs().pin_set().pinned().is_empty());
    // The blob stays stored, an orphan GC reclaims (SPEC §9.5).
    assert!(
        db.blobs()
            .store()
            .contains(
                &db.blobs()
                    .put_blob(&b"orphan"[..], &PutBlobOptions::default())
                    .unwrap()
                    .0
            )
            .unwrap()
    );
}

/// Where a crash of `put(doc, blob)` happens.
#[derive(Debug, Clone, Copy)]
enum CrashAt {
    /// At this store call (and every later one fails): the put fails there.
    Store(usize),
    /// After the put returned, before anything else.
    Returned,
    /// After the commit that follows it.
    Committed,
}

/// Invariant 2: crash `put(doc, blob)` at `at`, reopen, and check every document's blob is in the store.
fn crash_put(level: Durability, blob: &dyn Fn() -> Blob<'static>, at: CrashAt) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.persia");
    let store = FaultyStore::new(Faults::default());
    let blobs = Blobs::with_upload_ids(Arc::clone(&store), SegmentIds::seeded(7));
    let mut db = BlobDatabase::new(database(&path), blobs);
    // An earlier document, durable, whose blob a stored reference may reuse.
    db.put(
        "c",
        &doc("before"),
        Blob::from_reader(&bytes(100, 1)[..]),
        Durability::Fsync,
    )
    .unwrap();
    if let CrashAt::Store(n) = at {
        store.set_faults(Faults {
            crash_at: Some(n),
            ..Faults::default()
        });
    }
    let put = db.put("c", &doc("crashed"), blob(), level);
    if matches!(at, CrashAt::Committed) {
        db.commit().unwrap();
    }
    // The crash: buffered records are lost with the process; written ones survive (the file is not truncated: the
    // container keeps at least what a machine crash would).
    drop(db);
    let reopened = Database::open(&path, DatabaseOptions::default()).unwrap();
    let docs = documents(&reopened);
    for d in &docs {
        let r = d.blob_ref().unwrap();
        assert!(
            store.inner.contains(r).unwrap(),
            "{level:?} {at:?}: document {} references a missing blob",
            d.id()
        );
    }
    let survived = docs.iter().any(|d| d.id() == "crashed");
    if put.is_err() {
        assert!(
            !survived,
            "{level:?} {at:?}: a failed put logged its document"
        );
    }
    if put.is_ok() && (level == Durability::Fsync || matches!(at, CrashAt::Committed)) {
        assert!(
            survived,
            "{level:?} {at:?}: an acknowledged document was lost"
        );
    }
    put.is_ok()
}

/// Builds the blob of each attempt.
type MakeBlob = Box<dyn Fn() -> Blob<'static>>;

#[test]
fn a_crash_at_any_point_of_put_leaves_no_document_without_its_blob() {
    let blobs: [(&str, MakeBlob); 3] = [
        (
            "small",
            Box::new(|| {
                Blob::from_reader(std::io::Cursor::new(bytes(1000, 2))).with_options(options())
            }),
        ),
        (
            "large",
            Box::new(|| {
                Blob::from_reader(std::io::Cursor::new(bytes(
                    3 * ChunkSize::MIN as usize + 5,
                    3,
                )))
                .with_options(options())
            }),
        ),
        (
            "duplicate",
            Box::new(|| {
                Blob::from_reader(std::io::Cursor::new(bytes(100, 1))).with_options(options())
            }),
        ),
    ];
    for level in LEVELS {
        for (name, blob) in &blobs {
            let mut failed = 0;
            let mut n = 0_usize;
            // Each store call in turn until the put gets through.
            while !crash_put(level, blob.as_ref(), CrashAt::Store(n)) {
                failed += 1;
                n += 1;
                assert!(n < 64, "{name}: the put never got through");
            }
            assert!(failed > 0, "{name}");
            for at in [CrashAt::Returned, CrashAt::Committed] {
                assert!(crash_put(level, blob.as_ref(), at), "{name} {at:?}");
            }
        }
    }
}

#[test]
fn a_crash_while_checking_a_stored_reference_logs_nothing() {
    for level in LEVELS {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.persia");
        let store = FaultyStore::new(Faults::default());
        let mut db = BlobDatabase::new(database(&path), Blobs::new(Arc::clone(&store)));
        let r = db
            .put(
                "c",
                &doc("before"),
                Blob::from_reader(&b"stored"[..]),
                Durability::Fsync,
            )
            .unwrap();
        store.set_faults(Faults {
            crash_at: Some(0),
            ..Faults::default()
        });
        assert!(db.put("c", &doc("again"), Blob::Stored(r), level).is_err());
        assert!(store.calls().iter().any(|c| matches!(c, Call::Manifest(_))));
        drop(db);
        let docs = documents(&Database::open(&path, DatabaseOptions::default()).unwrap());
        assert_eq!(
            docs.iter().map(Document::id).collect::<Vec<_>>(),
            ["before"],
            "{level:?}"
        );
    }
}

/// A log whose put appends the record, then fails, as a put whose WAL sync fails does.
struct FailsAfterAppending(Database);

impl DocumentLog for FailsAfterAppending {
    fn put(
        &mut self,
        collection: &str,
        doc: &Document,
        _: Durability,
    ) -> Result<(), DatabaseError> {
        self.0.put(collection, doc, Durability::Buffered)?;
        Err(DatabaseError::Poisoned)
    }

    fn wal_mark(&self) -> WalMark {
        self.0.wal_mark()
    }

    fn durable_mark(&self) -> WalMark {
        self.0.durable_mark()
    }
}

/// Invariant 4: a record that may have reached the WAL keeps its blob pinned although its put failed.
#[test]
fn a_put_that_fails_after_appending_keeps_its_pins() {
    let dir = tempfile::tempdir().unwrap();
    let log = FailsAfterAppending(database(&dir.path().join("a.persia")));
    let mut db = BlobDatabase::new(log, Blobs::new(Arc::new(MemoryBlobStore::new())));
    let error = db
        .put(
            "c",
            &doc("x"),
            Blob::from_reader(&b"maybe logged"[..]),
            Durability::Fsync,
        )
        .unwrap_err();
    assert!(matches!(error, Error::Database(_)));
    assert_eq!(db.held(), 1);
    assert!(
        db.blobs()
            .pin_set()
            .is_pinned(Pinned::Blob(blob_id_of(b"maybe logged")))
    );
}

/// A bind whose outcome stays unknown fails the put: nothing is logged, nothing discarded.
#[test]
fn a_put_whose_bind_stays_unknown_logs_nothing() {
    for lands in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.persia");
        let store = FaultyStore::new(Faults {
            unknown_binds: u32::MAX,
            unknown_lands: lands,
            ..Faults::default()
        });
        let mut db = BlobDatabase::new(database(&path), Blobs::new(Arc::clone(&store)));
        let before = db.log().wal_mark();
        let data = bytes(200_000, 8);
        let blob = Blob::from_reader(&data[..]).with_options(PutBlobOptions {
            bind_attempts: 3,
            ..options()
        });
        let Err(Error::Blob(e)) = db.put("c", &doc("x"), blob, Durability::Fsync) else {
            panic!("the put went through");
        };
        assert!(e.is_bind_outcome_unknown());
        assert_eq!(db.log().wal_mark(), before);
        assert!(!store.calls().iter().any(|c| matches!(c, Call::Discard(_))));
        drop(db);
        assert!(documents(&Database::open(&path, DatabaseOptions::default()).unwrap()).is_empty());
    }
}

/// A stored reference whose size or chunk size differs from the bound blob's is corrupt and logs nothing.
#[test]
fn a_stored_reference_that_differs_from_its_manifest_logs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = BlobDatabase::new(
        database(&dir.path().join("a.persia")),
        Blobs::new(Arc::new(MemoryBlobStore::new())),
    );
    let (r, _pins) = db.put_blob(&b"0123456789"[..], &options()).unwrap();
    for (size, chunk) in [(11, r.chunk_size()), (10, r.chunk_size() + 1)] {
        let wrong =
            BlobRef::new(r.blob_id(), size, r.mime(), chunk, r.user_meta().clone()).unwrap();
        let before = db.log().wal_mark();
        let Err(Error::Blob(e)) = db.put("c", &doc("x"), Blob::Stored(wrong), Durability::Fsync)
        else {
            panic!("a mismatched reference was logged");
        };
        assert!(e.is_corruption());
        assert_eq!(db.log().wal_mark(), before);
    }
    assert!(documents(db.log()).is_empty());
}

/// Invariant 4 for one reference logged twice: two `Blob::Stored` puts of the same blob at `Buffered` hold its pins
/// twice, and only the commit that makes both records durable drops them to zero.
#[test]
fn two_stored_puts_of_one_reference_hold_its_pins_until_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = BlobDatabase::new(
        database(&dir.path().join("a.persia")),
        Blobs::new(Arc::new(MemoryBlobStore::new())),
    );
    let (r, guard) = db.put_blob(&bytes(300, 4)[..], &options()).unwrap();
    drop(guard);
    assert!(db.blobs().pin_set().pinned().is_empty());
    let upload = db
        .blobs()
        .store()
        .manifest(r.blob_id())
        .unwrap()
        .unwrap()
        .upload_id();
    let ids = [Pinned::Blob(r.blob_id()), Pinned::Upload(upload)];
    for (n, id) in ["x", "y"].into_iter().enumerate() {
        db.put("c", &doc(id), Blob::Stored(r.clone()), Durability::Buffered)
            .unwrap();
        assert_eq!(db.held(), n + 1);
        let mut pinned = db.blobs().pin_set().pinned();
        pinned.sort();
        let mut want = ids.to_vec();
        want.sort();
        assert_eq!(pinned, want, "after put {n}");
    }
    // A release with nothing durable releases nothing.
    db.release_durable();
    assert_eq!(db.held(), 2);
    assert!(ids.iter().all(|id| db.blobs().pin_set().is_pinned(*id)));
    db.commit().unwrap();
    assert_eq!(db.held(), 0);
    assert!(db.blobs().pin_set().pinned().is_empty());
}

/// What a step of [`pins_follow_durability_through_random_operations`] does.
#[derive(Debug, Clone)]
enum PinOp {
    /// A put at this durability (0 `Buffered`, 1 `Async`, 2 `Fsync`) of this blob.
    Put(u8, BlobChoice),
    /// A put to a collection that does not exist: rejected before its record is appended.
    Rejected(u8),
    /// A put whose blob reader fails: nothing stored, nothing logged.
    ReaderFails(u8),
    Commit,
    /// An `Fsync` delete through `log_mut` (durable for the engine); then `release_durable` or not.
    DeleteFsync(bool),
    /// `put_blob` alone, its guard kept.
    PutBlobKept(u8),
    /// Drops the oldest kept guard.
    DropGuard,
}

#[derive(Debug, Clone)]
enum BlobChoice {
    /// New bytes: this seed, small (`false`) or staged in chunks (`true`).
    New(u8, bool),
    /// The bytes of an earlier blob again, from a reader (a duplicate).
    Again(usize),
    /// The reference of an earlier blob (`Blob::Stored`).
    Stored(usize),
}

fn arb_pin_op() -> impl proptest::strategy::Strategy<Value = PinOp> {
    use proptest::prelude::*;
    let level = 0..3_u8;
    let choice = prop_oneof![
        3 => (any::<u8>(), proptest::bool::weighted(0.15)).prop_map(|(s, l)| BlobChoice::New(s, l)),
        1 => any::<usize>().prop_map(BlobChoice::Again),
        2 => any::<usize>().prop_map(BlobChoice::Stored),
    ];
    prop_oneof![
        8 => (level.clone(), choice).prop_map(|(l, c)| PinOp::Put(l, c)),
        1 => level.clone().prop_map(PinOp::Rejected),
        1 => level.prop_map(PinOp::ReaderFails),
        2 => Just(PinOp::Commit),
        2 => any::<bool>().prop_map(PinOp::DeleteFsync),
        1 => any::<u8>().prop_map(PinOp::PutBlobKept),
        1 => Just(PinOp::DropGuard),
    ]
}

fn level_of(n: u8) -> Durability {
    LEVELS[usize::from(n % 3)]
}

fn blob_bytes(seed: u8, large: bool) -> Vec<u8> {
    bytes(
        if large {
            70_000
        } else {
            50 + usize::from(seed)
        },
        seed,
    )
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        cases: if cfg!(feature = "slow") { 512 } else { 32 },
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x5045_5253_4941_0941),
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// Invariant 4 against a model that knows only the durability rules (SPEC §8.2.1): after every step, each
    /// document logged since the last durable point has its blob and the upload holding the blob's chunks pinned
    /// (what GC must not sweep, SPEC §9.5); `held` counts exactly the documents not yet released (a release happens
    /// at each put, commit and `release_durable`); and with nothing held and no `put_blob` guard alive, nothing is
    /// pinned (no pin leaks, also through failed puts).
    #[test]
    fn pins_follow_durability_through_random_operations(
        ops in proptest::collection::vec(arb_pin_op(), 1..40),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = BlobDatabase::new(
            database(&dir.path().join("a.persia")),
            Blobs::with_upload_ids(Arc::new(MemoryBlobStore::new()), SegmentIds::seeded(5)),
        );
        let mut stored: Vec<(BlobRef, Vec<u8>)> = Vec::new();
        // Blobs of the documents logged since the last durable point.
        let mut pending: Vec<BlobRef> = Vec::new();
        // Documents whose pins are held; and whether everything held is durable for the engine (not yet released).
        let (mut held, mut all_durable) = (0_usize, false);
        let mut guards = std::collections::VecDeque::new();
        for (step, op) in ops.iter().enumerate() {
            let ctx = format!("step {step} {op:?}");
            // `put` releases what is durable first, whatever happens next.
            let release = |held: &mut usize, all_durable: &mut bool| {
                if *all_durable {
                    *held = 0;
                    *all_durable = false;
                }
            };
            match op {
                PinOp::Put(l, choice) => {
                    let level = level_of(*l);
                    let blob = match choice {
                        BlobChoice::New(seed, large) => {
                            Blob::from_reader(std::io::Cursor::new(blob_bytes(*seed, *large)))
                                .with_options(options())
                        }
                        BlobChoice::Again(k) | BlobChoice::Stored(k) if stored.is_empty() => {
                            let _ = k;
                            Blob::from_reader(std::io::Cursor::new(blob_bytes(0, false)))
                                .with_options(options())
                        }
                        BlobChoice::Again(k) => Blob::from_reader(std::io::Cursor::new(
                            stored[k % stored.len()].1.clone(),
                        ))
                        .with_options(options()),
                        BlobChoice::Stored(k) => Blob::Stored(stored[k % stored.len()].0.clone()),
                    };
                    let bytes_of = match choice {
                        BlobChoice::New(seed, large) => Some(blob_bytes(*seed, *large)),
                        _ if stored.is_empty() => Some(blob_bytes(0, false)),
                        _ => None,
                    };
                    let r = db.put("c", &doc(&format!("d{}", step % 5)), blob, level).unwrap();
                    if let Some(b) = bytes_of {
                        stored.push((r.clone(), b));
                    }
                    release(&mut held, &mut all_durable);
                    if level == Durability::Fsync {
                        pending.clear();
                        held = 0;
                    } else {
                        pending.push(r);
                        held += 1;
                    }
                }
                PinOp::Rejected(l) => {
                    let blob = Blob::from_reader(&b"rejected"[..]);
                    let e = db.put("nope", &doc("x"), blob, level_of(*l)).unwrap_err();
                    proptest::prop_assert!(matches!(e, Error::Database(_)), "{}: {}", ctx, e);
                    release(&mut held, &mut all_durable);
                }
                PinOp::ReaderFails(l) => {
                    struct Fails;
                    impl Read for Fails {
                        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                            Err(std::io::Error::other("the reader failed"))
                        }
                    }
                    let blob = Blob::from_reader(Fails);
                    let e = db.put("c", &doc("x"), blob, level_of(*l)).unwrap_err();
                    proptest::prop_assert!(matches!(e, Error::Blob(_)), "{}: {}", ctx, e);
                    release(&mut held, &mut all_durable);
                }
                PinOp::Commit => {
                    db.commit().unwrap();
                    pending.clear();
                    held = 0;
                    all_durable = false;
                }
                PinOp::DeleteFsync(then_release) => {
                    db.log_mut().delete("c", "d0", Durability::Fsync).unwrap();
                    pending.clear();
                    all_durable = held > 0;
                    if *then_release {
                        db.release_durable();
                        release(&mut held, &mut all_durable);
                    }
                }
                PinOp::PutBlobKept(seed) => {
                    let (r, pins) = db.put_blob(&blob_bytes(*seed, false)[..], &options()).unwrap();
                    stored.push((r, blob_bytes(*seed, false)));
                    guards.push_back(pins);
                }
                PinOp::DropGuard => {
                    guards.pop_front();
                }
            }
            let set = db.blobs().pin_set();
            for r in &pending {
                let upload = db.blobs().store().manifest(r.blob_id()).unwrap().unwrap().upload_id();
                proptest::prop_assert!(set.is_pinned(Pinned::Blob(r.blob_id())), "{}: blob unpinned", ctx);
                proptest::prop_assert!(set.is_pinned(Pinned::Upload(upload)), "{}: upload unpinned", ctx);
            }
            for g in &guards {
                for id in g.ids() {
                    proptest::prop_assert!(set.is_pinned(*id), "{}: a live guard's {:?}", ctx, id);
                }
            }
            proptest::prop_assert_eq!(db.held(), held, "{}", ctx);
            if held == 0 && guards.is_empty() {
                proptest::prop_assert!(set.pinned().is_empty(), "{}: leaked {:?}", ctx, set.pinned());
            }
        }
    }
}

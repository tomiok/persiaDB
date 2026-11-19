//! A hit carries its document's blob reference (SPEC §7.5.1, §9.1; ROADMAP 9.4.3), wherever the document is: in the
//! memtable, in a flushed segment, in a merged one, and after a WAL replay; through a plain search and a relaxed one,
//! with any projection. Each segment's `BlobRefs` section agrees with its rows (SPEC §5.3.6).
//!
//! Invariants:
//! 1. Every hit's `document.blob_ref()` is the `BlobRef` its document was put with (`None` without one), after a
//!    flush, a merge, a reopen that replays the WAL, and an upsert that changes or removes it.
//! 2. `Segment::blob_ref(docid)` (the `BlobRefs` section) equals the row's for every document of every live segment,
//!    and a segment none of whose documents has a blob has no `BlobRefs` section's records to read.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, SearchRequest, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::query::{DefaultFields, Query, QueryCompiler, QueryLimits};
use persia_engine::relax::{RelaxOptions, RelaxPlan};
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::{Projection, Pruning, SearchPlan};
use persia_engine::wal::Durability;
use persia_format::{BlobId, BlobRef};
use persia_storage::container::sim::{CrashLoss, SimFile};

fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(9),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        ..DatabaseOptions::default()
    }
}

fn blob(seed: u8) -> BlobRef {
    let meta = BTreeMap::from([("filename".to_owned(), format!("f{seed}.bin"))]);
    BlobRef::new(
        BlobId::from_bytes([seed; 32]),
        u64::from(seed) * 1000,
        "application/pdf",
        64 << 10,
        meta,
    )
    .unwrap()
}

fn doc(id: &str, text: &str, blob: Option<BlobRef>) -> Document {
    let body = schema().field("body").unwrap().id();
    let mut d = Document::new(id).with(body, Value::String(text.into()));
    d.set_blob_ref(blob);
    d
}

type Expected = BTreeMap<String, Option<BlobRef>>;

fn put(db: &mut Database<SimFile>, expected: &mut Expected, d: &Document) {
    db.put("c", d, Durability::Fsync).unwrap();
    expected.insert(d.id().to_owned(), d.blob_ref().cloned());
}

/// Invariant 1 over a plain search of every document (all three projections) and a relaxed one.
fn check_hits(db: &Database<SimFile>, expected: &Expected, stage: &str) {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let plan = SearchPlan::new(
        &Query::MatchAll,
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    for projection in [
        Projection::All,
        Projection::None,
        Projection::fields(["body"], c.schema()).unwrap(),
    ] {
        let request = SearchRequest {
            limit: 100,
            projection,
            ..SearchRequest::default()
        };
        let results = views.search(&plan, &request, Pruning::default()).unwrap();
        // One hit per live document: no superseded version, no document twice.
        assert_eq!(
            results.hits.len(),
            expected.len(),
            "{stage}: {:?}",
            request.projection
        );
        let locations: std::collections::BTreeSet<_> = results
            .hits
            .iter()
            .map(|h| format!("{:?}", h.location))
            .collect();
        assert_eq!(
            locations.len(),
            results.hits.len(),
            "{stage}: a docid twice"
        );
        let got: Expected = results
            .hits
            .iter()
            .map(|h| (h.document.id().to_owned(), h.document.blob_ref().cloned()))
            .collect();
        assert_eq!(&got, expected, "{stage}: {:?}", request.projection);
    }
    // "red purple" matches nothing at L0 (every term required); dropping a term finds the red documents.
    let compiler = QueryCompiler::new(c.schema(), &AnalyzerRegistry::builtin()).unwrap();
    let query = compiler.compile(
        "red purple",
        &DefaultFields::all(c.schema()),
        &QueryLimits::default(),
    );
    let relax = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::default()).unwrap();
    let request = SearchRequest {
        limit: 100,
        ..SearchRequest::default()
    };
    let relaxed = views
        .search_relaxed(&query, &request, &relax, &Execution::default())
        .unwrap();
    assert!(
        !relaxed.hits.is_empty(),
        "{stage}: the relaxed search found nothing"
    );
    let ids: std::collections::BTreeSet<_> =
        relaxed.hits.iter().map(|h| h.hit.document.id()).collect();
    assert_eq!(
        ids.len(),
        relaxed.hits.len(),
        "{stage}: a relaxed hit twice"
    );
    for hit in &relaxed.hits {
        let d = &hit.hit.document;
        assert!(hit.step > 0, "{stage}: {} at L0", d.id());
        assert_eq!(
            Some(&d.blob_ref().cloned()),
            expected.get(d.id()),
            "{stage}: relaxed {}",
            d.id()
        );
    }
}

/// Invariant 2.
fn check_sections(db: &Database<SimFile>, stage: &str) -> usize {
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    for segment in c.segments() {
        for docid in 0..segment.doc_count() {
            let row = segment.document(docid).unwrap();
            assert_eq!(
                segment.blob_ref(docid).unwrap().as_ref(),
                row.blob_ref(),
                "{stage}: docid {docid}"
            );
        }
    }
    c.segments().len()
}

#[test]
fn hits_carry_blob_refs_through_flush_merge_and_replay() {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [3; 16], options()).unwrap();
    db.create_collection(schema()).unwrap();
    let mut expected = Expected::new();

    put(
        &mut db,
        &mut expected,
        &doc("a", "red shoes", Some(blob(1))),
    );
    put(&mut db, &mut expected, &doc("b", "red hat", None));
    put(&mut db, &mut expected, &doc("c", "blue sky", Some(blob(3))));
    check_hits(&db, &expected, "memtable");

    db.commit().unwrap();
    assert_eq!(check_sections(&db, "flush"), 1);
    check_hits(&db, &expected, "flush");

    // A second segment, with an upsert that changes one reference and one that removes another.
    put(
        &mut db,
        &mut expected,
        &doc("a", "red shoes again", Some(blob(4))),
    );
    put(&mut db, &mut expected, &doc("c", "blue sky", None));
    put(
        &mut db,
        &mut expected,
        &doc("d", "red apple", Some(blob(5))),
    );
    check_hits(&db, &expected, "memtable over a segment");
    db.commit().unwrap();
    assert_eq!(check_sections(&db, "second flush"), 2);
    check_hits(&db, &expected, "second flush");

    let merged = db.merge_step().unwrap();
    assert_eq!(merged.len(), 1, "{merged:?}");
    assert_eq!(check_sections(&db, "merge"), 1);
    check_hits(&db, &expected, "merge");

    // Logged, not committed: the reopen replays them into the memtable.
    put(&mut db, &mut expected, &doc("e", "red car", Some(blob(6))));
    put(
        &mut db,
        &mut expected,
        &doc("d", "red apple", Some(blob(7))),
    );
    drop(db);
    let crashed = file.crash(CrashLoss::All);
    let mut db = Database::open_on(crashed, options()).unwrap();
    check_hits(&db, &expected, "replay");
    db.commit().unwrap();
    check_sections(&db, "replayed flush");
    check_hits(&db, &expected, "replayed flush");
}

#[test]
fn a_segment_without_blobs_reads_none() {
    let mut db = Database::create_on(SimFile::new(), [4; 16], options()).unwrap();
    db.create_collection(schema()).unwrap();
    let mut expected = Expected::new();
    put(&mut db, &mut expected, &doc("x", "red", None));
    db.commit().unwrap();
    check_sections(&db, "no blobs");
    check_hits(&db, &expected, "no blobs");
}

/// `Database::durable_mark` reaches an operation's `wal_mark` once it is durable: at once for `Fsync`, at the next
/// commit or `Fsync` operation for `Buffered` and `Async` (the hook `put(doc, blob)` releases its pins on, SPEC §9.5).
#[test]
fn an_operation_is_durable_once_the_durable_mark_reaches_its_wal_mark() {
    let mut db = Database::create_on(SimFile::new(), [5; 16], options()).unwrap();
    db.create_collection(schema()).unwrap();
    for level in [Durability::Buffered, Durability::Async] {
        db.put("c", &doc("x", "red", Some(blob(1))), level).unwrap();
        let mark = db.wal_mark();
        assert!(db.durable_mark() < mark, "{level:?}");
        db.commit().unwrap();
        assert!(db.durable_mark() >= mark, "{level:?}");
        db.put("c", &doc("y", "red", None), level).unwrap();
        let mark = db.wal_mark();
        assert!(db.durable_mark() < mark, "{level:?}");
        db.delete("c", "nothing", Durability::Fsync).unwrap();
        assert!(db.durable_mark() >= mark, "{level:?}");
    }
    db.put("c", &doc("z", "red", None), Durability::Fsync)
        .unwrap();
    assert_eq!(db.durable_mark(), db.wal_mark());
}

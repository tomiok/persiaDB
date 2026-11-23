//! A schema that names an analyzer registered with `AnalyzerRegistry::register` (ROADMAP 6.6.8, SPEC §6.2.2).
//!
//! Invariants:
//! 1. A database opened with a registry holding the analyzer indexes and searches with it, through the memtable,
//!    a flushed segment and the replayed WAL, read-write and read-only.
//! 2. An open whose registry lacks it fails with `UnregisteredAnalyzer`, not corruption, before anything is replayed
//!    and before any segment is read, whichever collection names it, read-write and read-only, and so does a refresh
//!    that takes a commit creating such a collection; the built-ins are there whatever registry the caller gives.
//! 3. `open_skipping_analysis` opens it anyway, replays and verifies every record, lists every field it skipped (each
//!    one, also when several share the analyzer), and reaches the same documents. It still validates every record.
//! 4. Evolution (SPEC §6.2.4): an unversioned registered name binds to the field's version, a pinned other version
//!    is an analyzer change, and a field added with an analyzer the database lacks is refused, committing nothing.
//! 5. Segments built by a replay-time flush and by a merge, inline or on a worker, are indexed with the registered
//!    analyzer (the flush and the merge get the database's registry).
//! 6. Crash safety: for a fault at every mutating call of a workload (create, puts, commits, an evolution adding a
//!    registered-analyzer field) and a crash, an open without the analyzer is `UnregisteredAnalyzer` (or finds no
//!    collection), never corruption, and writes nothing beyond the container's own tail recovery; an open with it
//!    finds every acknowledged document through the registered analyzer.
//! 7. Property: a schema naming random registered analyzers round-trips through its stored JSON and through a
//!    manifest's encode/decode, with the same hash; a well-formed stored analyzer is never corruption.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::indexing_slicing,
    clippy::assert_is_empty,
    clippy::result_large_err,
    clippy::struct_excessive_bools,
    missing_docs
)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use persia_analysis::{AnalyzerRegistry, EdgeNgramFilter, LowercaseFilter, WhitespaceTokenizer};
use persia_engine::db::{
    CommitPolicy, Database, DatabaseError, DatabaseOptions, ManualClock, ReadOnlyDatabase,
    SearchRequest, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, FieldSet, Value};
use persia_engine::manifest::{CollectionDiff, CollectionEntry, Location, Manifest, ManifestDiff};
use persia_engine::memtable::{FlushPolicy, MemtableError};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::schema::{CollectionSchema, EvolutionError, FieldKind};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::segment::SegmentError;
use persia_engine::view::SearchView;
use persia_engine::wal::{Durability, ReplayError, WalRecord, WalWriter};
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Container, ContainerFile, Mode};
use proptest::prelude::*;

/// `prefix@1`: lowercased whitespace words and their 2- to 4-character prefixes, which no built-in indexes.
fn registry() -> AnalyzerRegistry {
    let mut registry = AnalyzerRegistry::new();
    let analyzer = persia_analysis::Analyzer::builder(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .filter(
            EdgeNgramFilter::new(NonZeroUsize::new(2).unwrap(), NonZeroUsize::new(4).unwrap())
                .unwrap()
                .preserve_original(true),
        )
        .build();
    registry.register("prefix", 1, analyzer).unwrap();
    registry
}

const SCHEMA: &str = r#"{"name": "notes", "fields": {
    "title": {"type": "text", "analyzer": "prefix"},
    "body": {"type": "text"}
}}"#;

fn options(registry: AnalyzerRegistry) -> DatabaseOptions {
    DatabaseOptions {
        registry,
        ..DatabaseOptions::default()
    }
}

/// `n1` committed in a segment, `n2` synced in the WAL after it.
fn database(path: &Path) {
    let schema = CollectionSchema::from_json_in(SCHEMA, &registry()).unwrap();
    let title = schema.field("title").unwrap().id();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create(path, options(registry())).unwrap();
    db.create_collection(schema).unwrap();
    let doc = |id: &str, t: &str| {
        Document::new(id)
            .with(title, Value::String(t.into()))
            .with(body, Value::String(format!("body of {id}")))
    };
    db.put("notes", &doc("n1", "Persia Database"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    db.put("notes", &doc("n2", "Pers Notes"), Durability::Fsync)
        .unwrap();
}

/// The ids `text` finds in `notes`, analyzed with `registry`.
fn search(snapshot: &Snapshot, registry: &AnalyzerRegistry, text: &str) -> Vec<String> {
    let notes = snapshot.collection("notes").unwrap();
    let compiler = QueryCompiler::new(notes.schema(), registry).unwrap();
    let query = compiler
        .compile(
            text,
            &DefaultFields::all(notes.schema()),
            &QueryLimits::default(),
        )
        .into_query();
    let memtable = notes.memtable();
    let plan =
        SearchPlan::new(&query, notes.schema(), &notes.search_statistics(&memtable)).unwrap();
    let page = notes
        .views(&memtable)
        .unwrap()
        .search(&plan, &SearchRequest::default(), Pruning::default())
        .unwrap();
    let mut ids: Vec<_> = page
        .hits
        .iter()
        .map(|h| h.document.id().to_owned())
        .collect();
    ids.sort();
    ids
}

#[test]
fn a_registered_analyzer_indexes_and_searches_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    database(&path);
    // The registry holds `prefix` only: the built-in `standard` of `body` is there anyway.
    let all = registry().with_builtins();
    let db = Database::open(&path, options(registry())).unwrap();
    // `pers` is a prefix `prefix@1` indexed for both, in a segment and in the replayed WAL.
    assert_eq!(search(&db.snapshot(), &all, "pers"), ["n1", "n2"]);
    assert_eq!(search(&db.snapshot(), &all, "data"), ["n1"]);
    assert_eq!(search(&db.snapshot(), &all, "body"), ["n1", "n2"]);
    drop(db);
    let ro = ReadOnlyDatabase::open(&path, &registry()).unwrap();
    assert_eq!(ro.replayed(), 1);
    assert!(ro.skipped_analysis().is_empty());
    assert_eq!(search(&ro.snapshot(), &all, "pers"), ["n1", "n2"]);
}

#[test]
fn an_open_without_the_analyzer_fails_as_unregistered_not_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    database(&path);
    let check = |e: DatabaseError| {
        assert!(!e.is_corruption(), "{e}");
        let DatabaseError::UnregisteredAnalyzer {
            collection,
            field,
            analyzer,
        } = &e
        else {
            panic!("{e:?}")
        };
        assert_eq!(
            (collection.as_str(), field.as_str(), analyzer.as_str()),
            ("notes", "title", "prefix@1")
        );
        assert!(e.to_string().contains("not registered"), "{e}");
    };
    for registry in [AnalyzerRegistry::builtin(), AnalyzerRegistry::new()] {
        check(ReadOnlyDatabase::open(&path, &registry).unwrap_err());
        check(Database::open(&path, options(registry)).unwrap_err());
    }
    // Another version of the same name is another analyzer.
    let mut other = AnalyzerRegistry::new();
    let analyzer = persia_analysis::Analyzer::builder(WhitespaceTokenizer).build();
    other.register("prefix", 2, analyzer).unwrap();
    check(ReadOnlyDatabase::open(&path, &other).unwrap_err());
    // Nothing was written: the database still opens with its analyzer.
    Database::open(&path, options(registry())).unwrap();
}

#[test]
fn an_unregistered_analyzer_fails_the_open_before_an_earlier_collection_is_read() {
    // `archive` (built-ins only) comes first in the manifest and has a damaged segment; `notes`, after it, names
    // `prefix@1`. The analyzers of every collection are checked before any segment is read (SPEC §6.2.2).
    let file = SimFile::new();
    {
        let mut db =
            Database::create_on(file.clone(), [9; 16], sim_options(registry(), BIG, 0)).unwrap();
        let archive = CollectionSchema::from_json(
            r#"{"name": "archive", "fields": {"body": {"type": "text"}}}"#,
        )
        .unwrap();
        let body = archive.field("body").unwrap().id();
        db.create_collection(archive).unwrap();
        db.create_collection(CollectionSchema::from_json_in(SCHEMA, &registry()).unwrap())
            .unwrap();
        let doc = Document::new("a1").with(body, Value::String("an archived page".into()));
        db.put("archive", &doc, Durability::Fsync).unwrap();
        let doc = note(&db.snapshot(), "n1", "Persia");
        db.put("notes", &doc, Durability::Fsync).unwrap();
        db.commit().unwrap();
        let names: Vec<_> = db
            .manifest()
            .collections()
            .map(CollectionEntry::name)
            .collect();
        assert_eq!(names, ["archive", "notes"]);
    }
    let mut bytes = file.bytes();
    {
        let container =
            Container::open_on(SimFile::from_bytes(bytes.clone()), Mode::ReadOnly).unwrap();
        let manifest = ReadOnlyDatabase::open_container(container, &registry())
            .unwrap()
            .snapshot()
            .manifest()
            .clone();
        let archive = manifest.collections().next().unwrap();
        let Location::Frame { offset, len } = archive.segments()[0].location else {
            panic!("a container segment is a frame")
        };
        let middle = usize::try_from(offset + len / 2).unwrap();
        bytes[middle] ^= 0xFF;
    }
    let damaged = || SimFile::from_bytes(bytes.clone());
    // With the analyzer, the open reads `archive`'s segment and finds the damage.
    let with = ReadOnlyDatabase::open_container(
        Container::open_on(damaged(), Mode::ReadOnly).unwrap(),
        &registry(),
    )
    .unwrap_err();
    assert!(with.is_corruption(), "{with:?}");
    let check = |e: DatabaseError| {
        assert!(!e.is_corruption(), "{e:?}");
        assert!(
            matches!(&e, DatabaseError::UnregisteredAnalyzer { collection, field, analyzer }
                if (collection.as_str(), field.as_str(), analyzer.as_str())
                    == ("notes", "title", "prefix@1")),
            "{e:?}"
        );
    };
    check(
        ReadOnlyDatabase::open_container(
            Container::open_on(damaged(), Mode::ReadOnly).unwrap(),
            &AnalyzerRegistry::builtin(),
        )
        .unwrap_err(),
    );
    check(Database::open_on(damaged(), sim_options(AnalyzerRegistry::new(), BIG, 0)).unwrap_err());
}

#[test]
fn a_refresh_checks_every_collections_analyzers_before_it_opens_a_segment() {
    // A reader of `archive` with an empty WAL (so the refresh is incremental, ADR-0060 item 3) follows a commit that
    // adds an `archive` segment, then one that creates `notes` with `prefix@1`. The new `archive` segment, first in
    // the manifest, is damaged: the refresh checks `notes`'s analyzers before it opens it, as an open would.
    let file = SimFile::new();
    let mut db =
        Database::create_on(file.clone(), [8; 16], sim_options(registry(), BIG, 0)).unwrap();
    let archive =
        CollectionSchema::from_json(r#"{"name": "archive", "fields": {"body": {"type": "text"}}}"#)
            .unwrap();
    let body = archive.field("body").unwrap().id();
    let page = |id: &str| Document::new(id).with(body, Value::String(format!("page {id}")));
    db.create_collection(archive).unwrap();
    db.put("archive", &page("a1"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    let image = file.bytes();
    let readers = [AnalyzerRegistry::builtin(), registry()].map(|registry| {
        let file = SimFile::from_bytes(image.clone());
        let reader = ReadOnlyDatabase::open_on(file.clone(), &registry).unwrap();
        assert_eq!(reader.replayed(), 0);
        (file, reader)
    });
    let old_segments = db
        .manifest()
        .collections()
        .next()
        .unwrap()
        .segments()
        .to_vec();
    db.put("archive", &page("a2"), Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.create_collection(CollectionSchema::from_json_in(SCHEMA, &registry()).unwrap())
        .unwrap();
    let mut bytes = file.bytes();
    {
        let archive = db.manifest().collections().next().unwrap();
        assert_eq!(archive.name(), "archive");
        let added = archive
            .segments()
            .iter()
            .find(|s| !old_segments.contains(s))
            .unwrap();
        let Location::Frame { offset, len } = added.location else {
            panic!("a container segment is a frame")
        };
        bytes[usize::try_from(offset + len / 2).unwrap()] ^= 0xFF;
    }
    drop(db);
    let refresh = |(file, reader): &(SimFile, ReadOnlyDatabase<SimFile>)| {
        file.write_all_at(&bytes, 0).unwrap();
        file.set_len(bytes.len() as u64).unwrap();
        let before = (reader.seq(), reader.manifest(), reader.replayed());
        let e = reader.refresh().unwrap_err();
        assert_eq!((reader.seq(), reader.manifest(), reader.replayed()), before);
        e
    };
    let e = refresh(&readers[0]);
    assert!(!e.is_corruption(), "{e:?}");
    assert!(
        matches!(&e, DatabaseError::UnregisteredAnalyzer { collection, field, analyzer }
            if (collection.as_str(), field.as_str(), analyzer.as_str())
                == ("notes", "title", "prefix@1")),
        "{e:?}"
    );
    // With the analyzer, the refresh opens the new segment and finds the damage.
    let with = refresh(&readers[1]);
    assert!(with.is_corruption(), "{with:?}");
}

#[test]
fn an_open_skipping_analysis_verifies_and_lists_the_skipped_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    database(&path);
    let ro = ReadOnlyDatabase::open_skipping_analysis(&path, &AnalyzerRegistry::new()).unwrap();
    let found = ro.skipped_analysis();
    let skipped: Vec<_> = found
        .iter()
        .map(|s| (s.collection.as_str(), s.field.as_str(), s.analyzer.as_str()))
        .collect();
    assert_eq!(skipped, [("notes", "title", "prefix@1")]);
    assert_eq!(ro.replayed(), 1);
    let report = ro.verify().unwrap();
    assert_eq!((report.documents, report.replayed), (1, 1));
    // Every document is there; only the skipped field's WAL terms are not.
    let snapshot = ro.snapshot();
    let notes = snapshot.collection("notes").unwrap();
    assert!(notes.get("n1").unwrap().is_some());
    assert!(notes.get("n2").unwrap().is_some());
    let builtin = AnalyzerRegistry::builtin();
    assert!(QueryCompiler::new(notes.schema(), &builtin).is_err());
    // With the analyzer, nothing is skipped.
    let full = ReadOnlyDatabase::open_skipping_analysis(&path, &registry()).unwrap();
    assert!(full.skipped_analysis().is_empty());
}

// ---- Every skipped field is listed (invariant 3) ----

/// Two collections, three fields naming `prefix@1` and one naming `other@1`, beside built-in fields.
const SCHEMAS: [&str; 2] = [
    r#"{"name": "notes", "fields": {
        "title": {"type": "text", "analyzer": "prefix"},
        "tags": {"type": "text", "analyzer": "prefix"},
        "body": {"type": "text"}
    }}"#,
    r#"{"name": "tasks", "fields": {
        "name": {"type": "text", "analyzer": "prefix"},
        "label": {"type": "text", "analyzer": "other"},
        "note": {"type": "text", "analyzer": "en"}
    }}"#,
];

/// `registry()` with `other@1` too.
fn registry_with_other() -> AnalyzerRegistry {
    let mut registry = registry();
    let analyzer = persia_analysis::Analyzer::builder(WhitespaceTokenizer).build();
    registry.register("other", 1, analyzer).unwrap();
    registry
}

/// SPEC §8.6 (proposed text of ROADMAP 6.6.8): `open_skipping_analysis` names *each* field whose analyzer it lacks,
/// in collection then field id order, also when several fields, in one collection or in several, share that analyzer.
#[test]
fn an_open_skipping_analysis_lists_every_field_of_a_missing_analyzer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let schemas: Vec<_> = SCHEMAS
        .iter()
        .map(|json| CollectionSchema::from_json_in(json, &registry_with_other()).unwrap())
        .collect();
    {
        let mut db = Database::create(&path, options(registry_with_other())).unwrap();
        for schema in &schemas {
            db.create_collection(schema.clone()).unwrap();
        }
    }
    // What the open must list for `held`: every text field, in order, whose analyzer `held` lacks.
    let expected = |held: &AnalyzerRegistry| -> Vec<(String, String, String)> {
        let held = held.clone().with_builtins();
        let mut out = Vec::new();
        for schema in &schemas {
            for field in schema.fields() {
                let FieldKind::Text(o) = field.kind() else {
                    continue;
                };
                if held.get(o.analyzer.name(), o.analyzer.version()).is_none() {
                    out.push((
                        schema.name().to_owned(),
                        field.name().to_owned(),
                        o.analyzer.to_string(),
                    ));
                }
            }
        }
        out
    };
    for held in [AnalyzerRegistry::new(), registry(), registry_with_other()] {
        let ro = ReadOnlyDatabase::open_skipping_analysis(&path, &held).unwrap();
        let skipped: Vec<_> = ro
            .skipped_analysis()
            .iter()
            .map(|s| (s.collection.clone(), s.field.clone(), s.analyzer.clone()))
            .collect();
        assert_eq!(skipped, expected(&held));
    }
    assert_eq!(expected(&AnalyzerRegistry::new()).len(), 4);
}

// ---- Evolution (invariant 4) ----

/// `registry()` with `prefix@2` too: lowercased whitespace words, no prefixes.
fn registry_v2() -> AnalyzerRegistry {
    let mut registry = registry();
    let analyzer = persia_analysis::Analyzer::builder(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .build();
    registry.register("prefix", 2, analyzer).unwrap();
    registry
}

/// `SCHEMA` with `extra` added after `body`.
fn with_field(extra: &str) -> String {
    let body = r#""body": {"type": "text"}"#;
    assert!(SCHEMA.contains(body));
    SCHEMA.replace(body, &format!("{body}, {extra}"))
}

fn analyzer_of(schema: &CollectionSchema, field: &str) -> persia_engine::schema::AnalyzerRef {
    let Some(FieldKind::Text(o)) = schema.field(field).map(persia_engine::schema::Field::kind)
    else {
        panic!("{field}")
    };
    o.analyzer.clone()
}

/// SPEC §6.2.4: re-applying the definition `"analyzer": "prefix"` once `prefix@2` is registered is not a change
/// (it binds to the field's `prefix@1`); pinning `prefix@2` is; a field added with an analyzer the database's
/// registry lacks is refused before anything is committed; one added with a registered analyzer is indexed with it.
#[test]
fn evolution_binds_an_unversioned_registered_analyzer_to_the_field_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    database(&path);
    let both = registry_v2();
    let mut db = Database::open(&path, options(both.clone())).unwrap();
    let seq = db.seq();

    // Read with a registry whose highest `prefix` is 2, the definition names `prefix@2`, not pinned...
    let proposed = CollectionSchema::from_json_in(SCHEMA, &both).unwrap();
    let title = analyzer_of(&proposed, "title");
    assert_eq!(
        (title.to_string().as_str(), title.is_pinned()),
        ("prefix@2", false)
    );
    // ...which binds to the field's version: no change, no commit.
    assert_eq!(db.evolve_collection(&proposed).unwrap(), seq);
    let pinned_v1 = SCHEMA.replace(r#""prefix""#, r#""prefix@1""#);
    let pinned_v1 = CollectionSchema::from_json_in(&pinned_v1, &both).unwrap();
    assert_eq!(db.evolve_collection(&pinned_v1).unwrap(), seq);

    // A pinned other version is another analyzer.
    let pinned = SCHEMA.replace(r#""prefix""#, r#""prefix@2""#);
    let e = db
        .evolve_collection(&CollectionSchema::from_json_in(&pinned, &both).unwrap())
        .unwrap_err();
    let DatabaseError::Evolution {
        collection,
        source:
            EvolutionError::FieldChanged {
                field,
                property,
                current,
                proposed,
            },
    } = &e
    else {
        panic!("{e:?}")
    };
    assert_eq!(
        [collection, field, *property, current, proposed],
        ["notes", "title", "analyzer", "prefix@1", "prefix@2"]
    );
    assert_eq!(db.seq(), seq);

    // A field naming `other@1`, which only the definition's registry holds, cannot be indexed: refused, nothing
    // committed.
    let added = with_field(r#""label": {"type": "text", "analyzer": "other"}"#);
    let mut definitions = both.clone();
    let other = persia_analysis::Analyzer::builder(WhitespaceTokenizer).build();
    definitions.register("other", 1, other).unwrap();
    let e = db
        .evolve_collection(&CollectionSchema::from_json_in(&added, &definitions).unwrap())
        .unwrap_err();
    assert!(
        matches!(
            &e,
            DatabaseError::Memtable(MemtableError::Segment(SegmentError::UnknownAnalyzer {
                field,
                analyzer,
            })) if field == "label" && analyzer == "other@1"
        ),
        "{e:?}"
    );
    assert!(!e.is_corruption());
    assert_eq!(db.seq(), seq);
    assert!(
        db.snapshot()
            .collection("notes")
            .unwrap()
            .schema()
            .field("label")
            .is_none()
    );

    // A field added as `"prefix"` gets the highest registered version, and is indexed with it.
    let added = with_field(r#""tags": {"type": "text", "analyzer": "prefix"}"#);
    let evolved = db
        .evolve_collection(&CollectionSchema::from_json_in(&added, &both).unwrap())
        .unwrap();
    assert_eq!(evolved, seq + 1);
    let schema = db.snapshot().collection("notes").unwrap().schema().clone();
    assert_eq!(analyzer_of(&schema, "title").to_string(), "prefix@1");
    assert_eq!(analyzer_of(&schema, "tags").to_string(), "prefix@2");
    let doc = Document::new("n3")
        .with(
            schema.field("title").unwrap().id(),
            Value::String("Lamp".into()),
        )
        .with(
            schema.field("tags").unwrap().id(),
            Value::String("Quartz".into()),
        );
    db.put("notes", &doc, Durability::Fsync).unwrap();
    let all = both.clone().with_builtins();
    // `prefix@2` indexes no prefixes: `qua` finds nothing, `quartz` finds `n3`; `lam` is a `prefix@1` title prefix.
    assert!(search(&db.snapshot(), &all, "qua").is_empty());
    assert_eq!(search(&db.snapshot(), &all, "quartz"), ["n3"]);
    assert_eq!(search(&db.snapshot(), &all, "lam"), ["n3"]);
    db.commit().unwrap();
    drop(db);

    // The evolved schema names `prefix@2`: `prefix@1` alone does not open it any more.
    let e = Database::open(&path, options(registry())).unwrap_err();
    assert!(
        matches!(&e, DatabaseError::UnregisteredAnalyzer { field, analyzer, .. }
            if field == "tags" && analyzer == "prefix@2"),
        "{e:?}"
    );
    let db = Database::open(&path, options(both)).unwrap();
    assert_eq!(search(&db.snapshot(), &all, "quartz"), ["n3"]);
    assert_eq!(search(&db.snapshot(), &all, "pers"), ["n1", "n2"]);
}

// ---- Replay-time flushes and merges (invariant 5) ----

const BIG: usize = 64 << 20;

/// Titles whose lowercase 3-letter prefixes are distinct: only `prefix@1` indexes `qua` for `Quartz`.
const WORDS: [(&str, &str); 6] = [
    ("w0", "Quartz"),
    ("w1", "Zephyr"),
    ("w2", "Marble"),
    ("w3", "Violet"),
    ("w4", "Copper"),
    ("w5", "Salmon"),
];

fn sim_options(
    registry: AnalyzerRegistry,
    memtable_bytes: usize,
    threads: usize,
) -> DatabaseOptions {
    DatabaseOptions {
        registry,
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(11),
        policy: CommitPolicy {
            flush: FlushPolicy {
                memtable_bytes,
                ..FlushPolicy::default()
            },
            ..CommitPolicy::default()
        },
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads,
        },
        ..DatabaseOptions::default()
    }
}

/// A `notes` document titled `word`, with the field ids of `snapshot`'s schema.
fn note(snapshot: &Snapshot, id: &str, word: &str) -> Document {
    let schema = snapshot.collection("notes").unwrap().schema().clone();
    Document::new(id)
        .with(
            schema.field("title").unwrap().id(),
            Value::String(word.into()),
        )
        .with(
            schema.field("body").unwrap().id(),
            Value::String(format!("body of {id}")),
        )
}

/// Every word's 3-letter prefix finds exactly its document if it is in `live`, nothing otherwise; `maybe` may be
/// found or not (an operation whose outcome its caller does not know).
fn check_prefixes(snapshot: &Snapshot, ctx: &str, live: &BTreeSet<&str>, maybe: Option<&str>) {
    let all = registry().with_builtins();
    for (id, word) in WORDS {
        let prefix = word.chars().take(3).collect::<String>().to_lowercase();
        let hits = search(snapshot, &all, &prefix);
        if live.contains(id) {
            assert_eq!(hits, [id], "{ctx}: {prefix}");
        } else if maybe == Some(id) {
            assert!(hits.is_empty() || hits == [id], "{ctx}: {prefix}: {hits:?}");
        } else {
            assert!(hits.is_empty(), "{ctx}: {prefix}: {hits:?}");
        }
    }
}

/// SPEC §8.6, ROADMAP 6.6.5: a backlog replayed at an open whose memtable threshold it reaches is flushed by the
/// replay, into segments built with the registered analyzer; a merge of them, inline or on a worker, keeps its terms.
#[test]
fn replay_flushes_and_merges_index_with_the_registered_analyzer() {
    let ids: BTreeSet<&str> = WORDS.iter().map(|(id, _)| *id).collect();
    for threads in [0, 1] {
        let ctx = format!("{threads} merge threads");
        let file = SimFile::new();
        {
            let schema = CollectionSchema::from_json_in(SCHEMA, &registry()).unwrap();
            let mut db =
                Database::create_on(file.clone(), [3; 16], sim_options(registry(), BIG, threads))
                    .unwrap();
            db.create_collection(schema).unwrap();
            for (id, word) in WORDS {
                let doc = note(&db.snapshot(), id, word);
                db.put("notes", &doc, Durability::Fsync).unwrap();
            }
            assert!(
                db.manifest()
                    .collection("notes")
                    .unwrap()
                    .segments()
                    .is_empty()
            );
        }
        // Every record after the first finds the memtable at the threshold: the replay commits it first.
        let mut db = Database::open_on(file.clone(), sim_options(registry(), 1, threads)).unwrap();
        check_prefixes(&db.snapshot(), &format!("{ctx}, replayed"), &ids, None);
        db.finish_merges().unwrap();
        let stats = db.merge_stats();
        assert!(
            stats.committed >= 1 && stats.failed == 0,
            "{ctx}: {stats:?}"
        );
        let segments = db.manifest().collection("notes").unwrap().segments().len();
        assert!(segments < WORDS.len() - 1, "{ctx}: {segments} segments");
        check_prefixes(&db.snapshot(), &format!("{ctx}, merged"), &ids, None);
        drop(db);
        let db = Database::open_on(file, sim_options(registry(), BIG, threads)).unwrap();
        check_prefixes(&db.snapshot(), &format!("{ctx}, reopened"), &ids, None);
    }
}

// ---- Crash safety (invariant 6) ----

fn fault(at: u64, apply_first: bool) -> SimFault {
    if apply_first {
        SimFault::ApplyThenFail(at)
    } else {
        SimFault::FailAt(at)
    }
}

/// What a crash workload acknowledged, and the put it was in when it failed.
#[derive(Default)]
struct Acked {
    created: bool,
    puts: BTreeSet<&'static str>,
    attempted: Option<&'static str>,
}

/// Create `notes`, put `w0`, commit, put `w1`, evolve (adding `tags`, `prefix@1`), put `w2`, commit, put `w3`: the
/// last put is left in the WAL. Stops at the first error.
fn crash_workload(db: &mut Database<SimFile>, acked: &mut Acked) -> Result<(), DatabaseError> {
    let schema = CollectionSchema::from_json_in(SCHEMA, &registry()).unwrap();
    db.create_collection(schema)?;
    acked.created = true;
    let mut put = |db: &mut Database<SimFile>, (id, word): (&'static str, &str)| {
        acked.attempted = Some(id);
        let doc = note(&db.snapshot(), id, word);
        db.put("notes", &doc, Durability::Fsync)?;
        acked.puts.insert(id);
        acked.attempted = None;
        Ok::<(), DatabaseError>(())
    };
    put(db, WORDS[0])?;
    db.commit()?;
    put(db, WORDS[1])?;
    let evolved = with_field(r#""tags": {"type": "text", "analyzer": "prefix"}"#);
    db.evolve_collection(&CollectionSchema::from_json_in(&evolved, &registry()).unwrap())?;
    put(db, WORDS[2])?;
    db.commit()?;
    put(db, WORDS[3])?;
    Ok(())
}

/// Invariant 6 on the crash image `bytes`; returns which states it covered.
fn check_crash_image(ctx: &str, bytes: &[u8], acked: &Acked) -> Seen {
    // With the analyzer: every acknowledged document, found through `prefix@1`.
    let db = Database::open_on(
        SimFile::from_bytes(bytes.to_vec()),
        sim_options(registry(), BIG, 0),
    )
    .unwrap_or_else(|e| panic!("{ctx}: open with the analyzer: {e}"));
    let snapshot = db.snapshot();
    let has_notes = snapshot.collection("notes").is_some();
    let mut seen = Seen {
        notes: has_notes,
        tail: snapshot
            .collection("notes")
            .is_some_and(|c| c.memtable().doc_count() > 0),
        stray_manifest: stray_manifest(bytes),
        unregistered: false,
    };
    if has_notes {
        check_prefixes(&snapshot, ctx, &acked.puts, acked.attempted);
    } else {
        assert!(!acked.created && acked.puts.is_empty(), "{ctx}: notes lost");
    }
    drop((db, snapshot));

    let unregistered = |e: &DatabaseError, how: &str| {
        assert!(!e.is_corruption(), "{ctx}: {how}: {e}");
        assert!(
            matches!(e, DatabaseError::UnregisteredAnalyzer { collection, field, analyzer }
                if collection == "notes" && field == "title" && analyzer == "prefix@1"),
            "{ctx}: {how}: {e:?}"
        );
    };
    // Without it, a writer open fails before replaying, and writes nothing the container's own open does not.
    let reference = SimFile::from_bytes(bytes.to_vec());
    drop(Container::open_on(reference.clone(), Mode::ReadWrite).unwrap());
    let without = SimFile::from_bytes(bytes.to_vec());
    match Database::open_on(
        without.clone(),
        sim_options(AnalyzerRegistry::new(), BIG, 0),
    ) {
        Ok(db) => {
            assert!(!has_notes, "{ctx}: opened without the analyzer");
            assert!(db.snapshot().collection("notes").is_none(), "{ctx}");
        }
        Err(e) => {
            assert!(has_notes, "{ctx}: {e}");
            unregistered(&e, "writer");
            seen.unregistered = true;
            assert!(
                without.bytes() == reference.bytes(),
                "{ctx}: the failed open wrote"
            );
        }
    }
    let container =
        Container::open_on(SimFile::from_bytes(bytes.to_vec()), Mode::ReadOnly).unwrap();
    match ReadOnlyDatabase::open_container(container, &AnalyzerRegistry::new()) {
        Ok(ro) => assert!(
            !has_notes && ro.snapshot().collection("notes").is_none(),
            "{ctx}"
        ),
        Err(e) => {
            assert!(has_notes, "{ctx}: {e}");
            unregistered(&e, "read-only");
        }
    }
    seen
}

/// What one crash image covered.
#[derive(Debug, Clone, Copy)]
struct Seen {
    /// `notes` is committed.
    notes: bool,
    /// The open with the analyzer replayed a document into the memtable.
    tail: bool,
    /// A valid Manifest frame after the committed `log_end` (a commit cut before its superblock slot, SPEC §4.5),
    /// which a successful writer open would retire with a commit.
    stray_manifest: bool,
    /// The writer open without the analyzer failed as unregistered.
    unregistered: bool,
}

fn stray_manifest(bytes: &[u8]) -> bool {
    let container =
        Container::open_on(SimFile::from_bytes(bytes.to_vec()), Mode::ReadOnly).unwrap();
    let log_end = container.superblock().log_end;
    container
        .frames()
        .iter()
        .any(|f| f.kind == persia_format::frame::FrameKind::Manifest && f.offset >= log_end)
}

/// Invariant 6 for a fault at every mutating call of the workload, failing or applied-then-failing, and a crash
/// that keeps no unsynced write or all of them.
#[test]
fn every_crash_point_opens_with_the_analyzer_and_fails_cleanly_without_it() {
    let calls = {
        let file = SimFile::new();
        let mut db =
            Database::create_on(file.clone(), [5; 16], sim_options(registry(), BIG, 0)).unwrap();
        let start = file.mutations();
        let mut acked = Acked::default();
        crash_workload(&mut db, &mut acked).unwrap();
        drop(db);
        // The clean run: the WAL tail `w3` is replayed, everything else is in segments.
        let seen = check_crash_image("no fault", &file.bytes(), &acked);
        assert!(seen.notes && seen.tail && seen.unregistered, "{seen:?}");
        file.mutations() - start
    };
    assert!(calls > 10, "{calls}");
    let mut images = Vec::new();
    for point in 0..calls {
        for apply_first in [false, true] {
            let file = SimFile::new();
            let mut db =
                Database::create_on(file.clone(), [5; 16], sim_options(registry(), BIG, 0))
                    .unwrap();
            file.set_fault(Some(fault(file.mutations() + point, apply_first)));
            let mut acked = Acked::default();
            let result = crash_workload(&mut db, &mut acked);
            drop(db);
            for loss in [CrashLoss::All, CrashLoss::Nothing] {
                let ctx = format!(
                    "fault at call {point} (apply first: {apply_first}), {loss:?}, workload: {result:?}"
                );
                images.push(check_crash_image(&ctx, &file.crash(loss).bytes(), &acked));
            }
        }
    }
    // The fault points reach every state: no collection yet, a committed one with and without a WAL tail, and a
    // cut commit whose Manifest the failed unregistered open must not retire.
    let count = |f: fn(&Seen) -> bool| images.iter().filter(|s| f(s)).count();
    let counts = [
        count(|s| !s.notes),
        count(|s| s.notes && s.tail),
        count(|s| s.notes && !s.tail),
        count(|s| s.unregistered && s.stray_manifest),
    ];
    eprintln!("{} images: {counts:?}", images.len());
    assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
}

// ---- Skipping analysis still validates (invariant 3) ----

/// ROADMAP 6.6.8 (proposed SPEC §8.6 text: "Replay still decodes each of its records, validates it against the
/// schema and applies it"): a synced `notes` put whose `title` is an integer fails an open that skips `title`'s
/// analysis exactly as it fails one that has the analyzer: the record does not decode for the schema.
#[test]
fn an_open_skipping_analysis_still_rejects_an_invalid_record() {
    let file = SimFile::new();
    let schema = CollectionSchema::from_json_in(SCHEMA, &registry()).unwrap();
    {
        let mut db =
            Database::create_on(file.clone(), [7; 16], sim_options(registry(), BIG, 0)).unwrap();
        db.create_collection(schema.clone()).unwrap();
        let doc = note(&db.snapshot(), "n1", "Persia");
        db.put("notes", &doc, Durability::Fsync).unwrap();
        db.commit().unwrap();
    }
    // A twin schema with the same field ids, `title` an `i64`, encodes the record no `notes` put could log.
    let twin = CollectionSchema::from_json(
        r#"{"name": "notes", "fields": {"title": {"type": "i64"}, "body": {"type": "text"}}}"#,
    )
    .unwrap();
    for name in ["title", "body"] {
        assert_eq!(
            twin.field(name).unwrap().id(),
            schema.field(name).unwrap().id()
        );
    }
    let bad = Document::new("n2")
        .with(twin.field("title").unwrap().id(), Value::I64(7))
        .with(
            twin.field("body").unwrap().id(),
            Value::String("body of n2".into()),
        )
        .encode(&twin, FieldSet::All)
        .unwrap();
    {
        let container = Container::open_on(file.clone(), Mode::ReadWrite).unwrap();
        let mut wal = WalWriter::resume(container, std::collections::BTreeMap::new());
        wal.append(&WalRecord::put("notes", &bad).unwrap(), Durability::Fsync)
            .unwrap();
    }
    let read_only = |skipping: bool, registry: &AnalyzerRegistry| {
        let container = Container::open_on(file.clone(), Mode::ReadOnly).unwrap();
        let opened = if skipping {
            ReadOnlyDatabase::open_container_skipping_analysis(container, registry)
        } else {
            ReadOnlyDatabase::open_container(container, registry)
        };
        opened.unwrap_err()
    };
    let with = read_only(false, &registry());
    assert!(
        matches!(with, DatabaseError::Replay(ReplayError::Document { .. })),
        "{with:?}"
    );
    for registry in [AnalyzerRegistry::new(), registry()] {
        let skipping = read_only(true, &registry);
        assert!(
            matches!(
                skipping,
                DatabaseError::Replay(ReplayError::Document { .. })
            ),
            "{skipping:?}"
        );
        assert_eq!(skipping.to_string(), with.to_string());
        assert_eq!(skipping.is_corruption(), with.is_corruption());
    }
    // A writer with the analyzer refuses it too.
    let writer = Database::open_on(file.clone(), sim_options(registry(), BIG, 0)).unwrap_err();
    assert_eq!(writer.to_string(), with.to_string());
}

// ---- Stored form (invariant 7) ----

/// A name `register` accepts that is not a built-in's: mostly from a small pool, so that one name often has several
/// versions and `"name"` must pick the highest.
fn registered_name() -> impl Strategy<Value = String> {
    let any_name = "[a-z][a-z0-9_-]{0,8}".prop_filter("a built-in name", |n| {
        persia_analysis::current_builtin_version(n).is_none()
    });
    prop_oneof![
        3 => prop::sample::select(vec!["tags", "x", "my-prefix_2"]).prop_map(str::to_owned),
        1 => any_name,
    ]
}

/// A version: small ones collide and order, large ones reach the varint and `u32` bounds.
fn version() -> impl Strategy<Value = u32> {
    prop_oneof![1u32..=4, 1u32..=u32::MAX]
}

proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(64),
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x0668),
        failure_persistence: None,
        ..proptest::test_runner::Config::default()
    })]

    /// SPEC §6.2.2, §6.2.5, §8.7: text fields naming registered analyzers, pinned or not, read with `from_json_in`,
    /// store their resolved `name@version`, read back through `from_json_in` and through a manifest (no registry)
    /// to the same schema and hash, and the manifest re-encodes byte-identically.
    #[test]
    fn registered_analyzers_round_trip_through_stored_json_and_manifests(
        fields in proptest::collection::vec(
            (registered_name(), version(), any::<bool>()),
            1..5,
        ),
    ) {
        let mut registry = AnalyzerRegistry::new();
        for (name, version, _) in &fields {
            let analyzer = persia_analysis::Analyzer::builder(WhitespaceTokenizer).build();
            // A repeated name and version is registered once.
            match registry.register(name, *version, analyzer) {
                Ok(()) | Err(persia_analysis::RegisterError::Exists { .. }) => {}
                Err(e) => return Err(TestCaseError::fail(format!("{name}@{version}: {e}"))),
            }
        }
        let mut json = String::from(r#"{"name": "c", "fields": {"plain": {"type": "text"}"#);
        for (i, (name, version, pinned)) in fields.iter().enumerate() {
            let analyzer = if *pinned { format!("{name}@{version}") } else { name.clone() };
            write!(json, r#", "f{i}": {{"type": "text", "analyzer": "{analyzer}"}}"#).unwrap();
        }
        json.push_str("}}");
        let schema = CollectionSchema::from_json_in(&json, &registry).unwrap();
        for (i, (name, version, pinned)) in fields.iter().enumerate() {
            let a = analyzer_of(&schema, &format!("f{i}"));
            let expected = if *pinned { *version } else { registry.current_version(name).unwrap() };
            prop_assert_eq!((a.name(), a.version(), a.is_pinned()), (name.as_str(), expected, *pinned));
            prop_assert!(!a.is_builtin());
        }

        // Stored JSON: pinned, read back only with the registry.
        let stored = schema.to_json().unwrap();
        let reread = CollectionSchema::from_json_in(&stored, &registry).unwrap();
        prop_assert_eq!(&reread, &schema);
        prop_assert_eq!(reread.hash(), schema.hash());
        let is_unknown = matches!(
            CollectionSchema::from_json(&stored),
            Err(persia_engine::schema::SchemaError::UnknownAnalyzer { .. })
        );
        prop_assert!(is_unknown);

        // A manifest holding it decodes without any registry, to the same schema, and re-encodes to itself.
        let mut diff = ManifestDiff::default();
        diff.collections.insert(
            "c".to_owned(),
            CollectionDiff::Create { schema: schema.clone(), segments: Vec::new() },
        );
        let manifest = Manifest::empty().apply(&diff).unwrap();
        let bytes = manifest.encode().unwrap();
        let decoded = Manifest::decode(&bytes).unwrap();
        prop_assert_eq!(&decoded, &manifest);
        let read = decoded.collections().next().unwrap().schema();
        prop_assert_eq!(read, &schema);
        prop_assert_eq!(read.hash(), schema.hash());
        for i in 0..fields.len() {
            let a = analyzer_of(read, &format!("f{i}"));
            prop_assert!(a.is_pinned() && !a.is_builtin());
        }
        prop_assert_eq!(decoded.encode().unwrap(), bytes);
    }
}

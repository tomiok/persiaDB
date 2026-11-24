//! When a read-only handle maps (SPEC §4.7, ADR-0017 amended 2026-11-23): the open maps its container, each refresh
//! that finds something new maps the one it reopened before it opens a segment from it, a refresh that finds nothing
//! maps nothing, and a mapping that fails is not an error. A simulated file cannot be mapped, so the mapping function
//! is replaced by ones that count their calls; the real one is tested in `tests/read_only_mapped.rs`.

#![allow(clippy::unwrap_used, clippy::panic)] // Tests: inputs are fixed.

use std::cell::Cell;

use persia_storage::container::sim::SimFile;

use super::*;
use crate::db::{Database, DatabaseOptions};
use crate::document::{Document, Value};
use crate::schema::{SchemaBuilder, TextOptions};
use crate::wal::Durability;

thread_local! {
    /// How many containers this thread's reader was asked to map.
    static MAPPED: Cell<usize> = const { Cell::new(0) };
}

/// Stands in for `Container::map_read_only_if_local` on a simulated file: counts, maps nothing.
#[allow(clippy::unnecessary_wraps)] // The signature of `MapReads`.
fn counts(_: &mut Container<SimFile>) -> persia_storage::container::Result<bool> {
    MAPPED.set(MAPPED.get() + 1);
    Ok(false)
}

/// A mapping that fails, as one without address space does.
fn fails(_: &mut Container<SimFile>) -> persia_storage::container::Result<bool> {
    MAPPED.set(MAPPED.get() + 1);
    Err(persia_storage::container::Error::InvalidInput(
        "no address space".into(),
    ))
}

fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn writer() -> (SimFile, Database<SimFile>) {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [9; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema()).unwrap();
    (file, db)
}

fn put(db: &mut Database<SimFile>, id: &str) {
    let body = schema().field("body").unwrap().id();
    let doc = Document::new(id).with(body, Value::String("text".into()));
    db.put("c", &doc, Durability::Fsync).unwrap();
}

fn reader(file: &SimFile, map: MapReads<SimFile>) -> ReadOnlyDatabase<SimFile> {
    let container = Container::open_on(file.clone(), Mode::ReadOnly).unwrap();
    ReadOnlyDatabase::open_as(
        container,
        &AnalyzerRegistry::builtin(),
        Analysis::Required,
        new_cache(),
        Some(map),
    )
    .unwrap()
}

/// The open maps its container, and each refresh that finds something maps the one it reopened; a refresh that
/// finds nothing maps nothing.
#[test]
fn a_refresh_maps_the_container_it_takes_and_no_other() {
    let (file, mut db) = writer();
    MAPPED.set(0);
    let reader = reader(&file, counts);
    assert_eq!(MAPPED.get(), 1);
    assert_eq!(reader.refresh().unwrap(), Refreshed::Unchanged);
    assert_eq!(MAPPED.get(), 1);
    put(&mut db, "a");
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Incremental { records: 1, .. }
    ));
    assert_eq!(MAPPED.get(), 2);
    db.commit().unwrap();
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Reloaded { .. }
    ));
    assert_eq!(MAPPED.get(), 3);
    assert_eq!(reader.refresh().unwrap(), Refreshed::Unchanged);
    assert_eq!(MAPPED.get(), 3);
}

/// A mapping that fails leaves a reader that reads, at the open and at every refresh.
#[test]
fn a_mapping_that_fails_is_not_an_error() {
    let (file, mut db) = writer();
    put(&mut db, "a");
    db.commit().unwrap();
    MAPPED.set(0);
    let reader = reader(&file, fails);
    assert_eq!(MAPPED.get(), 1);
    assert!(!reader.container().is_mapped());
    let get = |id: &str| reader.snapshot().collection("c").unwrap().get(id).unwrap();
    assert!(get("a").is_some());
    // Both kinds of refresh.
    put(&mut db, "b");
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Incremental { records: 1, .. }
    ));
    assert_eq!(MAPPED.get(), 2);
    assert!(get("b").is_some());
    db.commit().unwrap();
    assert!(matches!(
        reader.refresh().unwrap(),
        Refreshed::Reloaded { .. }
    ));
    assert_eq!(MAPPED.get(), 3);
    assert!(get("a").is_some() && get("b").is_some());
}

/// The generic opens never map, whatever the options say.
#[test]
fn the_opens_of_a_given_file_map_nothing() {
    let (file, mut db) = writer();
    let options = ReadOnlyOptions::new().mapped_reads(MappedReads::IfLocal);
    let on = ReadOnlyDatabase::open_on_with_options(file.clone(), &options).unwrap();
    let container = Container::open_on(file.clone(), Mode::ReadOnly).unwrap();
    let given = ReadOnlyDatabase::open_container_with_options(container, &options).unwrap();
    let older = ReadOnlyDatabase::open_on(file, &AnalyzerRegistry::builtin()).unwrap();
    for reader in [&on, &given, &older] {
        assert!(reader.map.is_none());
    }
    put(&mut db, "a");
    for reader in [&on, &given, &older] {
        reader.refresh().unwrap();
        assert!(reader.map.is_none());
    }
}

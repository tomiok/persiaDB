//! What a `Db` reads through (ROADMAP 10.2.4, 10.2.5; SPEC §4.7, §10.6; ADR-0017 amended): a read-only open maps its
//! container on a local disk by default (not on Windows), `OpenOptions::mapped_reads` can force ordinary reads, a
//! read-write `Db` never maps, and every `Db` reads its segments through a block cache of its own.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::path::Path;

use persia::{Db, Document, OpenOptions};
use persia_engine::db::{ReadOnlyDatabase, ReadOnlyOptions};

/// A committed database with one document in a segment, so a `get` reads a block.
fn committed(path: &Path) {
    let db = Db::open(path).unwrap();
    db.create_collection_json(
        r#"{"name": "c", "fields": {"t": {"type": "text", "stored": true}}}"#,
    )
    .unwrap()
    .put_json(r#"{"id": "a", "t": "stored text"}"#)
    .unwrap();
    db.close().unwrap();
}

#[test]
fn a_read_only_open_maps_by_default_on_a_local_disk_and_can_be_told_not_to() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    committed(&path);
    let local = ReadOnlyDatabase::open_with_options(&path, &ReadOnlyOptions::new())
        .unwrap()
        .container()
        .is_on_local_disk()
        .unwrap();

    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    // The default: mapped on a local disk, except on Windows (ADR-0017 amended).
    assert_eq!(reader.is_mapped().unwrap(), local && !cfg!(windows));
    reader.refresh().unwrap();
    assert_eq!(
        reader.is_mapped().unwrap(),
        local && !cfg!(windows),
        "a refresh keeps it"
    );
    assert!(reader.collection("c").unwrap().get("a").unwrap().is_some());

    let plain = OpenOptions::new()
        .read_only(true)
        .mapped_reads(false)
        .open(&path)
        .unwrap();
    assert!(!plain.is_mapped().unwrap(), "forced ordinary reads");
    let asked = OpenOptions::new()
        .read_only(true)
        .mapped_reads(true)
        .open(&path)
        .unwrap();
    assert_eq!(
        asked.is_mapped().unwrap(),
        local,
        "asked for, also on Windows"
    );

    drop((reader, plain, asked));
    let writer = OpenOptions::new().mapped_reads(true).open(&path).unwrap();
    assert!(!writer.is_mapped().unwrap(), "a read-write Db never maps");
}

#[test]
fn every_db_reads_through_a_cache_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    committed(&path);
    let options = OpenOptions::new()
        .read_only(true)
        .block_cache_bytes(1 << 20);
    let first = options.open(&path).unwrap();
    let second = options.open(&path).unwrap();
    // The open itself reads through the cache (the segment's own blocks); after it, each counts its own reads.
    let opened = second.block_cache_stats().unwrap();
    assert_eq!(opened.capacity, 1 << 20);
    let before = first.block_cache_stats().unwrap();
    let c = first.collection("c").unwrap();
    for _ in 0..3 {
        assert!(c.get("a").unwrap().is_some());
    }
    let used = first.block_cache_stats().unwrap();
    assert!(
        used.hits + used.misses > before.hits + before.misses && used.bytes > 0,
        "{before:?} -> {used:?}"
    );
    assert!(
        used.hits > before.hits,
        "a block read twice is a hit: {used:?}"
    );
    assert_eq!(second.block_cache_stats().unwrap(), opened, "not shared");

    // A read-write `Db` has its own too, of the default size.
    let writer = Db::open(dir.path().join("other.persia")).unwrap();
    assert_eq!(writer.block_cache_stats().unwrap().capacity, 8 << 20);
    writer.close().unwrap();
    first.close().unwrap();
    second.close().unwrap();
}

#[test]
fn a_read_write_db_reads_its_flushed_segments_through_its_cache() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = db
        .create_collection_json(
            r#"{"name": "c", "fields": {"t": {"type": "text", "stored": true}}}"#,
        )
        .unwrap();
    c.put(&Document::new("a")).unwrap();
    db.commit().unwrap();
    let before = db.block_cache_stats().unwrap();
    assert!(c.get("a").unwrap().is_some());
    let after = db.block_cache_stats().unwrap();
    assert!(
        after.hits + after.misses > before.hits + before.misses,
        "{before:?} -> {after:?}"
    );
}

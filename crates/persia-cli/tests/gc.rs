//! `persia gc` end to end (ROADMAP 10.4.5; SPEC §9.5): orphan blobs deleted and the pack rewritten smaller, referenced
//! blobs kept and `verify --deep` clean after it, the local pack's lack of ages (no grace-young object kept), a reader
//! lease behind the committed seq holding the sweep back, a database without a pack, dangling references reported,
//! the `--grace` checks, the `--json` keys and the text rows, a writer holding the database, and a schema naming a
//! registered analyzer (ROADMAP 10.4.7, ADR-0069: collected; ROADMAP 10.4.8: refused, writing nothing, while the WAL
//! holds a record of such a collection, in either order with a backlog of another collection that crosses the
//! memtable size, N times in a row, and collected once the application has closed it). The exit status of
//! `gc` for a sound, missing, non-container and damaged file is in `tests/commands.rs`'s table.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::panic
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use persia::internal::documents::{Blob, BlobDatabase, ReadOnlyBlobDatabase};
use persia_engine::analysis::{Analyzer, AnalyzerRegistry, WhitespaceTokenizer};
use persia_engine::db::{Database, DatabaseOptions};
use persia_engine::document::{Document, Value};
use persia_engine::schema::{CollectionSchema, SchemaBuilder};
use persia_engine::wal::Durability;
use serde_json::Value as Json;

/// As in `tests/commands.rs`: a spawned `persia` must not inherit a lock another test holds.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn persia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_persia"))
        .args(args)
        .output()
        .unwrap()
}

fn arg(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    assert!(out.status.success(), "{}", stderr(out));
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn json(out: &Output) -> Json {
    let text = stdout(out);
    assert_eq!(text.lines().count(), 1, "{text}");
    serde_json::from_str(&text).unwrap()
}

fn gc_json(path: &Path) -> Json {
    json(&persia(&["gc", arg(path), "--json"]))
}

fn keys(value: &Json) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn set<'a>(names: &[&'a str]) -> BTreeSet<&'a str> {
    names.iter().copied().collect()
}

/// The bytes of blob `n`: 64 KiB, distinct per `n`.
fn bytes_of(n: u8) -> Vec<u8> {
    (0..64 * 1024u32)
        .map(|i| u8::try_from(i % 251).unwrap() ^ n)
        .collect()
}

/// The pack beside the container at `path`.
fn pack(path: &Path) -> PathBuf {
    persia::internal::pack_path(path)
}

/// A closed database whose collection `files` held `f1`..`f4`, each with its own blob, and then deleted `f2`, `f3`
/// and `f4` and committed: one referenced blob, three orphans (three quarters of the pack's frames).
fn with_orphans(path: &Path) {
    let mut db = BlobDatabase::create(path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new("files").build().unwrap())
        .unwrap();
    for n in 1..=4u8 {
        let doc = Document::new(format!("f{n}"));
        let bytes = bytes_of(n);
        db.put(
            "files",
            &doc,
            Blob::from_reader(&bytes[..]),
            Durability::Fsync,
        )
        .unwrap();
    }
    db.commit().unwrap();
    for id in ["f2", "f3", "f4"] {
        db.log_mut().delete("files", id, Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
}

/// The blob `f1` references, read back whole from a reopened database.
fn f1_bytes(path: &Path) -> Vec<u8> {
    let db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    let snapshot = db.log().snapshot();
    let doc = snapshot
        .collection("files")
        .unwrap()
        .get("f1")
        .unwrap()
        .unwrap();
    let blob = doc.blob_ref().unwrap().clone();
    db.get_blob_range(&blob, 0..64 * 1024).unwrap()
}

#[test]
fn gc_deletes_the_orphans_rewrites_the_pack_smaller_and_keeps_the_referenced_blob() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    with_orphans(&path);
    let before = std::fs::metadata(pack(&path)).unwrap().len();

    let done = gc_json(&path);
    assert_eq!(done["held_back"], false, "{done}");
    assert_eq!(done["marked"]["blobs"], 1, "{done}");
    assert_eq!(done["deleted"]["blobs"], 3, "{done}");
    assert_eq!(done["missing"], serde_json::json!([]), "{done}");
    assert_eq!(done["rewrite"]["outcome"], "done", "{done}");
    let frames_before = done["rewrite"]["frame_bytes_before"].as_u64().unwrap();
    let frames_after = done["rewrite"]["frame_bytes_after"].as_u64().unwrap();
    assert!(frames_after < frames_before / 2, "{done}");
    assert_eq!(done["pack"]["present"], true);
    assert_eq!(done["pack"]["bytes_before"], before);
    let after = done["pack"]["bytes_after"].as_u64().unwrap();
    assert!(after < before / 2, "the pack shrank: {before} -> {after}");
    assert_eq!(std::fs::metadata(pack(&path)).unwrap().len(), after);
    // The local pack keeps no ages: the orphans, written a moment ago, were dropped with the default 24 h grace, and no
    // object counts as young (SPEC §9.3, §9.5).
    assert_eq!(done["grace_secs"], 24 * 3600);
    assert_eq!(done["kept"]["young"], 0, "{done}");

    assert_eq!(f1_bytes(&path), bytes_of(1));
    let verified = persia(&["verify", "--deep", arg(&path)]);
    assert!(verified.status.success(), "{}", stderr(&verified));

    // Again: nothing left to delete, nothing rewritten.
    let again = gc_json(&path);
    assert_eq!(again["deleted"]["blobs"], 0, "{again}");
    assert_eq!(again["marked"]["blobs"], 1, "{again}");
    assert_ne!(again["rewrite"]["outcome"], "done", "{again}");
}

/// The `--json` object has exactly the keys the README lists, at every level.
#[test]
fn gc_json_has_its_documented_shape() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    with_orphans(&path);
    let done = gc_json(&path);
    assert_eq!(
        keys(&done),
        set(&[
            "grace_secs",
            "held_back",
            "held_by_copy",
            "pack",
            "marked",
            "missing",
            "deleted",
            "kept",
            "rewrite",
            "readers"
        ])
    );
    assert_eq!(
        keys(&done["pack"]),
        set(&["present", "bytes_before", "bytes_after"])
    );
    assert_eq!(keys(&done["marked"]), set(&["blobs", "uploads"]));
    assert_eq!(keys(&done["deleted"]), set(&["blobs", "uploads", "chunks"]));
    assert_eq!(
        keys(&done["kept"]),
        set(&[
            "pinned",
            "young",
            "unknown_age",
            "changed",
            "unreadable",
            "uploads_skipped"
        ])
    );
    assert_eq!(
        keys(&done["rewrite"]),
        set(&[
            "outcome",
            "dead_bytes",
            "frame_bytes",
            "generation",
            "frame_bytes_before",
            "frame_bytes_after"
        ])
    );
    assert_eq!(
        keys(&done["readers"]),
        set(&[
            "live",
            "current",
            "opening",
            "behind",
            "ahead",
            "unreadable",
            "stale_removed",
            "oldest_seq"
        ])
    );
    assert_eq!(done["held_by_copy"], false);
    assert_eq!(done["readers"]["live"], 0, "{done}");
}

#[test]
fn gc_text_has_a_row_per_part_of_the_report() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    with_orphans(&path);
    let text = stdout(&persia(&["gc", arg(&path), "--grace", "2h"]));
    let rows: Vec<&str> = text.lines().map(|l| l.split_at(18).0.trim_end()).collect();
    assert_eq!(
        rows,
        [
            "grace", "pack", "sweep", "marked", "deleted", "kept", "rewrite", "readers"
        ],
        "{text}"
    );
    assert!(text.contains("grace             7200 s\n"), "{text}");
    assert!(text.contains("sweep             ran\n"), "{text}");
    assert!(
        text.contains("deleted           3 blobs, 0 uploads, "),
        "{text}"
    );
    assert!(
        text.contains("rewrite           done (generation "),
        "{text}"
    );
}

/// A read-only handle opened before the last commit holds a reader lease behind the committed seq: the sweep does
/// not run, nothing is deleted or rewritten, and the report says so. Once that handle is gone, the sweep runs.
#[test]
fn a_reader_lease_behind_the_committed_seq_holds_the_sweep_back() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    with_orphans(&path);
    let reader = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default()).unwrap();
    assert!(reader.lease().is_some(), "the reader holds a lease");
    // A commit after the reader's open puts it behind.
    let mut writer = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
    writer
        .log_mut()
        .put("files", &Document::new("plain"), Durability::Fsync)
        .unwrap();
    writer.commit().unwrap();
    drop(writer);
    let before = std::fs::read(pack(&path)).unwrap();

    let held = gc_json(&path);
    assert_eq!(held["held_back"], true, "{held}");
    assert_eq!(held["readers"]["live"], 1, "{held}");
    assert_eq!(held["readers"]["behind"], 1, "{held}");
    assert_eq!(held["readers"]["current"], 0, "{held}");
    assert_eq!(held["deleted"]["blobs"], 0, "{held}");
    assert_eq!(held["rewrite"]["outcome"], "not_run", "{held}");
    assert_eq!(held["marked"]["blobs"], 1, "the mark ran: {held}");
    assert_eq!(
        std::fs::read(pack(&path)).unwrap(),
        before,
        "the pack is untouched"
    );
    let text = stdout(&persia(&["gc", arg(&path)]));
    assert!(
        text.contains("sweep             held back by reader leases: nothing deleted\n"),
        "{text}"
    );
    assert!(
        text.contains("readers           1 live: 0 current, 0 opening, 1 behind"),
        "{text}"
    );

    drop(reader);
    let swept = gc_json(&path);
    assert_eq!(swept["held_back"], false, "{swept}");
    assert_eq!(swept["deleted"]["blobs"], 3, "{swept}");
    assert_eq!(swept["readers"]["live"], 0, "{swept}");
}

/// A database that never stored a blob has no pack: nothing to collect, status 0. No pack and no lease directory is
/// created; the writer's open takes the pack's writer lock, so its lock file `<name>.persia.blobs.lock` appears, as
/// with any writer that opens the database with its blob store (it is never removed, SPEC §9.3).
#[test]
fn a_database_without_a_pack_is_a_clean_no_op() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    db.create_collection(SchemaBuilder::new("files").build().unwrap())
        .unwrap();
    db.put("files", &Document::new("f"), Durability::Fsync)
        .unwrap();
    drop(db);
    let listing = || -> BTreeSet<PathBuf> {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect()
    };
    let before = listing();
    let done = gc_json(&path);
    assert_eq!(done["pack"]["present"], false, "{done}");
    assert_eq!(done["pack"]["bytes_before"], Json::Null);
    assert_eq!(done["rewrite"]["outcome"], "no_pack", "{done}");
    assert_eq!(done["readers"], Json::Null, "{done}");
    assert_eq!(done["deleted"]["blobs"], 0);
    assert_eq!(done["held_back"], false);
    let mut expected = before.clone();
    expected.insert(dir.path().join("app.persia.blobs.lock"));
    assert_eq!(
        listing(),
        expected,
        "only the pack's writer lock file is created"
    );
    let text = stdout(&persia(&["gc", arg(&path)]));
    assert!(text.contains("pack              none -> none\n"), "{text}");
    assert!(
        text.contains("rewrite           no pack: nothing to collect\n"),
        "{text}"
    );
}

/// A document whose blob the pack lacks (the container copied without its pack, SPEC §4.8) is a dangling reference:
/// GC lists it and deletes nothing it references; `verify` is what fails on it.
#[test]
fn a_dangling_reference_is_listed_as_missing() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    with_orphans(&path);
    let blob_id = {
        let db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
        let snapshot = db.log().snapshot();
        let doc = snapshot
            .collection("files")
            .unwrap()
            .get("f1")
            .unwrap()
            .unwrap();
        doc.blob_ref().unwrap().blob_id().to_string()
    };
    std::fs::remove_file(pack(&path)).unwrap();
    let done = gc_json(&path);
    assert_eq!(done["missing"], serde_json::json!([blob_id]), "{done}");
    assert_eq!(done["rewrite"]["outcome"], "no_pack", "{done}");
    let text = stdout(&persia(&["gc", arg(&path)]));
    assert!(
        text.contains(&format!("missing           {blob_id}\n")),
        "{text}"
    );
}

/// `--grace` is `<number><unit>`; a malformed one is a usage error (status 2). One below the GC's floor (2 h) is
/// refused by the GC itself before it reads anything (status 1), and the pack is untouched.
#[test]
fn the_grace_period_is_checked() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    with_orphans(&path);
    for bad in [
        "",
        "24",
        "h",
        "24 h",
        "1.5h",
        "24w",
        "-2h",
        "99999999999999999999d",
    ] {
        let out = persia(&["gc", arg(&path), "--grace", bad]);
        assert_eq!(out.status.code(), Some(2), "{bad:?}: {}", stderr(&out));
    }
    let before = std::fs::read(pack(&path)).unwrap();
    for below in ["1h", "119m", "7199s", "0s"] {
        let out = persia(&["gc", arg(&path), "--grace", below]);
        assert_eq!(out.status.code(), Some(1), "{below}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("twice the longest bind budget"),
            "{below}: {}",
            stderr(&out)
        );
        assert_eq!(out.stdout, b"");
    }
    assert_eq!(std::fs::read(pack(&path)).unwrap(), before);
    for (ok, secs) in [
        ("2h", 7200),
        ("120m", 7200),
        ("3d", 3 * 86_400),
        ("7200s", 7200),
    ] {
        let done = json(&persia(&["gc", arg(&path), "--grace", ok, "--json"]));
        assert_eq!(done["grace_secs"], secs, "{ok}");
    }
}

#[cfg(unix)]
#[test]
fn gc_fails_while_a_writer_holds_the_database() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    with_orphans(&path);
    let writer = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
    let before = std::fs::read(pack(&path)).unwrap();
    let out = persia(&["gc", arg(&path)]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert!(
        stderr(&out).contains("is open by a writer; collect it once that is closed"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(pack(&path)).unwrap(), before);
    drop(writer);
}

/// The registry of an application that registers `tags@1`, and its options.
fn tags_options() -> DatabaseOptions {
    let mut registry = AnalyzerRegistry::builtin();
    registry
        .register("tags", 1, Analyzer::builder(WhitespaceTokenizer).build())
        .unwrap();
    DatabaseOptions {
        registry,
        ..DatabaseOptions::default()
    }
}

/// The collection `notes`, whose `tags` field names the registered `tags@1` (not `strict`: extra fields pad documents).
fn tags_schema(options: &DatabaseOptions) -> CollectionSchema {
    CollectionSchema::from_json_in(
        r#"{"name": "notes", "strict": false, "fields": {"tags": {"type": "text", "analyzer": "tags"}}}"#,
        &options.registry,
    )
    .unwrap()
}

/// A document of `notes` with tags `"a b"`.
fn tagged(schema: &CollectionSchema, id: &str) -> Document {
    let tags = schema.field("tags").unwrap().id();
    Document::new(id).with(tags, Value::String("a b".into()))
}

/// The blob of document `id` of `notes`, read whole by the application (with `tags@1`) from a read-only open.
fn app_blob(path: &Path, id: &str) -> Vec<u8> {
    let ro = ReadOnlyBlobDatabase::open(path, &tags_options()).unwrap();
    let snapshot = ro.database().snapshot();
    let doc = snapshot
        .collection("notes")
        .unwrap()
        .get(id)
        .unwrap()
        .unwrap_or_else(|| panic!("{id} is gone"));
    let blob = doc.blob_ref().unwrap().clone();
    ro.get_blob_range(&blob, 0..blob.size()).unwrap()
}

/// ROADMAP 10.4.7, ADR-0069: the command has the built-in analyzers only, and collects a database whose schema names a
/// registered one anyway (the mark-only open, whose mark reads no term): the orphans go, the pack is rewritten, the
/// referenced blob stays and reads back for the application, and `verify --deep` passes.
#[test]
fn gc_collects_a_schema_naming_a_registered_analyzer() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let options = tags_options();
    let schema = tags_schema(&options);
    let mut db = BlobDatabase::create(&path, options).unwrap();
    db.log_mut().create_collection(schema.clone()).unwrap();
    for (id, n) in [("n", 9), ("gone1", 10), ("gone2", 11)] {
        let bytes = bytes_of(n);
        db.put(
            "notes",
            &tagged(&schema, id),
            Blob::from_reader(&bytes[..]),
            Durability::Fsync,
        )
        .unwrap();
    }
    db.commit().unwrap();
    for id in ["gone1", "gone2"] {
        db.log_mut().delete("notes", id, Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
    drop(db);
    let size_before = std::fs::metadata(pack(&path)).unwrap().len();

    let done = gc_json(&path);
    assert_eq!(done["marked"]["blobs"], 1, "{done}");
    assert_eq!(done["deleted"]["blobs"], 2, "{done}");
    assert_eq!(done["missing"], serde_json::json!([]), "{done}");
    assert_eq!(done["rewrite"]["outcome"], "done", "{done}");
    assert!(std::fs::metadata(pack(&path)).unwrap().len() < size_before);
    assert_eq!(app_blob(&path, "n"), bytes_of(9));
    let verified = persia(&["verify", "--deep", arg(&path)]);
    assert!(verified.status.success(), "{}", stderr(&verified));
}

/// ROADMAP 10.4.7, ADR-0069, flipped by ROADMAP 10.4.8: a blob referenced only by a WAL record of a collection whose
/// analysis the open skips. That record refuses the open (status 1, the hint to open and close the database with its
/// application): nothing is marked, swept or written, so the blob survives, the record is still a WAL record, and the
/// application reads the document and its blob. Once the application has closed it (committing the record), the
/// command collects: the sweep keeps the blob while it drops the orphans.
#[test]
fn a_blob_only_an_unflushed_record_of_a_skipped_collection_references_survives_gc() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let options = tags_options();
    let schema = tags_schema(&options);
    let mut db = BlobDatabase::create(&path, options).unwrap();
    db.log_mut().create_collection(schema.clone()).unwrap();
    for (id, n) in [("gone1", 10), ("gone2", 11), ("gone3", 12)] {
        let bytes = bytes_of(n);
        db.put(
            "notes",
            &tagged(&schema, id),
            Blob::from_reader(&bytes[..]),
            Durability::Fsync,
        )
        .unwrap();
    }
    db.commit().unwrap();
    for id in ["gone1", "gone2", "gone3"] {
        db.log_mut().delete("notes", id, Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
    // Logged and synced, never committed: only the WAL references this blob.
    let wal_bytes = bytes_of(13);
    db.put(
        "notes",
        &tagged(&schema, "wal"),
        Blob::from_reader(&wal_bytes[..]),
        Durability::Fsync,
    )
    .unwrap();
    drop(db);
    let stats = json(&persia(&["stats", arg(&path), "--json"]));
    assert_eq!(stats["collections"][0]["wal_documents"], 1, "{stats}");

    assert_gc_refused(&path, "the WAL record is pending");
    let stats = json(&persia(&["stats", arg(&path), "--json"]));
    assert_eq!(
        stats["collections"][0]["wal_documents"], 1,
        "still a WAL record: {stats}"
    );
    assert_eq!(app_blob(&path, "wal"), wal_bytes);

    close_with_the_application(&path);
    let done = gc_json(&path);
    assert_eq!(done["marked"]["blobs"], 1, "{done}");
    assert_eq!(done["deleted"]["blobs"], 3, "{done}");
    assert_eq!(done["rewrite"]["outcome"], "done", "{done}");
    assert_eq!(app_blob(&path, "wal"), wal_bytes);
    let verified = persia(&["verify", "--deep", arg(&path)]);
    assert!(verified.status.success(), "{}", stderr(&verified));
}

/// ROADMAP 10.4.7, the refusal (ADR-0069 item 4, ADR-0068 item 2; since ROADMAP 10.4.8 by the open's pre-scan): a WAL
/// backlog of such a collection larger than the default memtable, which only the analyzer can index. The command fails
/// with status 1 before it marks or sweeps anything, says why and what to do, and leaves the container and the pack as
/// they were; once the application has flushed the backlog, it collects.
#[test]
fn gc_refuses_to_flush_a_backlog_that_needs_a_registered_analyzer() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut options = tags_options();
    let schema = tags_schema(&options);
    options.policy.flush.memtable_bytes = 1 << 30;
    let mut db = BlobDatabase::create(&path, options).unwrap();
    db.log_mut().create_collection(schema.clone()).unwrap();
    let bytes = bytes_of(9);
    db.put(
        "notes",
        &tagged(&schema, "n"),
        Blob::from_reader(&bytes[..]),
        Durability::Fsync,
    )
    .unwrap();
    db.commit().unwrap();
    // 18 documents of 4 MiB each (an extra field, stored and not analyzed): 72 MiB, all in the WAL.
    let pad = Json::String("p".repeat(4 << 20));
    for i in 0..18 {
        let mut doc = tagged(&schema, &format!("big{i:02}"));
        doc.insert_extra("pad", pad.clone());
        db.log_mut()
            .put("notes", &doc, Durability::Buffered)
            .unwrap();
    }
    db.log_mut()
        .put("notes", &Document::new("last"), Durability::Fsync)
        .unwrap();
    drop(db);
    let before = (
        std::fs::read(&path).unwrap(),
        std::fs::read(pack(&path)).unwrap(),
    );
    let out = persia(&["gc", arg(&path), "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(out.stdout, b"");
    let err = stderr(&out);
    assert!(
        err.contains(r#"collection "notes" has writes in the WAL that only the analyzer tags@1 of field "tags" can index, and this open lacks it (it refuses before replaying them, writing nothing); open the database with its application and close it (closing a `persia::Db` commits them), then retry"#),
        "{err}"
    );
    assert!(
        err.contains("open the database with its application and close it, then retry"),
        "{err}"
    );
    let after = (
        std::fs::read(&path).unwrap(),
        std::fs::read(pack(&path)).unwrap(),
    );
    assert!(after == before, "the files are unchanged");

    let mut db = Database::open(&path, tags_options()).unwrap();
    db.commit().unwrap();
    drop(db);
    let done = gc_json(&path);
    assert_eq!(done["marked"]["blobs"], 1, "{done}");
}
/// `persia gc` exits 1 on `path` for a pending record of `notes` (ROADMAP 10.4.8), says what to do, writes no report,
/// and leaves the container and the pack byte-identical.
fn assert_gc_refused(path: &Path, ctx: &str) {
    let before = (
        std::fs::read(path).unwrap(),
        std::fs::read(pack(path)).unwrap(),
    );
    let out = persia(&["gc", arg(path), "--json"]);
    assert_eq!(out.status.code(), Some(1), "{ctx}: {}", stderr(&out));
    assert_eq!(out.stdout, b"", "{ctx}");
    let err = stderr(&out);
    assert!(
        err.contains("open the database with its application and close it, then retry"),
        "{ctx}: {err}"
    );
    assert!(err.contains(r#"collection "notes""#), "{ctx}: {err}");
    let after = (
        std::fs::read(path).unwrap(),
        std::fs::read(pack(path)).unwrap(),
    );
    assert!(
        after == before,
        "{ctx}: persia gc wrote to the container or the pack"
    );
}

/// `persia compact` exits 1 on `path` for a pending record of `notes` (ROADMAP 10.4.8), says what to do, writes no
/// report, and leaves the container and the pack byte-identical.
fn assert_compact_refused(path: &Path, ctx: &str) {
    let before = (
        std::fs::read(path).unwrap(),
        std::fs::read(pack(path)).unwrap(),
    );
    let out = persia(&["compact", arg(path), "--json"]);
    assert_eq!(out.status.code(), Some(1), "{ctx}: {}", stderr(&out));
    assert_eq!(out.stdout, b"", "{ctx}");
    let err = stderr(&out);
    assert!(
        err.contains("open the database with its application and close it, then retry"),
        "{ctx}: {err}"
    );
    let after = (
        std::fs::read(path).unwrap(),
        std::fs::read(pack(path)).unwrap(),
    );
    assert!(
        after == before,
        "{ctx}: persia compact wrote to the container or the pack"
    );
}

/// The application (with `tags@1`) opens the database at `path` and closes it, committing: what the refusal's hint
/// asks for.
fn close_with_the_application(path: &Path) {
    let mut db = BlobDatabase::open(path, tags_options()).unwrap();
    db.commit().unwrap();
}

/// The first 100 bytes of [`bytes_of`]`(n)`: a small blob, distinct per `n`, so the three 64 KiB orphans of
/// [`with_a_backlog_and_a_note`] are over the default rewrite ratio.
fn small_bytes(n: u8) -> Vec<u8> {
    bytes_of(n)[..100].to_vec()
}

/// Where the `notes` record sits relative to the `plain` backlog.
#[derive(Debug, Clone, Copy)]
enum Order {
    Before,
    After,
}

/// A closed database with `notes` (naming `tags@1`) and `plain` (built-in analyzers only): committed, `kept` with its
/// blob and three orphans (their documents deleted and committed); then, never committed, one `notes` document with
/// its blob and a `plain` backlog of 18 documents of 4 MiB each (an extra stored field, each with a small blob, 72
/// MiB: over the default 64 MiB memtable, so a writer's replay flushes it), the note before or after the backlog
/// (`order`). Returns the ids each collection holds and the blob every document references, by id.
fn with_a_backlog_and_a_note(path: &Path, order: Order) -> Vec<(&'static str, String, Vec<u8>)> {
    let mut options = tags_options();
    let notes = tags_schema(&options);
    options.policy.flush.memtable_bytes = 1 << 30;
    let mut db = BlobDatabase::create(path, options).unwrap();
    db.log_mut().create_collection(notes.clone()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new("plain").strict(false).build().unwrap())
        .unwrap();
    let mut docs = Vec::new();
    let mut put = |db: &mut BlobDatabase<_>,
                   collection: &'static str,
                   doc: Document,
                   bytes: Vec<u8>,
                   durability| {
        let id = doc.id().to_owned();
        db.put(collection, &doc, Blob::from_reader(&bytes[..]), durability)
            .unwrap();
        docs.push((collection, id, bytes));
    };
    put(
        &mut db,
        "notes",
        tagged(&notes, "kept"),
        bytes_of(1),
        Durability::Fsync,
    );
    for (n, id) in [(2, "g1"), (3, "g2"), (4, "g3")] {
        let bytes = bytes_of(n);
        db.put(
            "notes",
            &tagged(&notes, id),
            Blob::from_reader(&bytes[..]),
            Durability::Fsync,
        )
        .unwrap();
    }
    db.commit().unwrap();
    for id in ["g1", "g2", "g3"] {
        db.log_mut().delete("notes", id, Durability::Fsync).unwrap();
    }
    db.release_durable();
    db.commit().unwrap();
    if let Order::Before = order {
        put(
            &mut db,
            "notes",
            tagged(&notes, "wal"),
            small_bytes(5),
            Durability::Buffered,
        );
    }
    let pad = Json::String("p".repeat(4 << 20));
    for i in 0..18_u8 {
        let mut doc = Document::new(format!("big{i:02}"));
        doc.insert_extra("pad", pad.clone());
        put(
            &mut db,
            "plain",
            doc,
            small_bytes(20 + i),
            Durability::Buffered,
        );
    }
    if let Order::After = order {
        put(
            &mut db,
            "notes",
            tagged(&notes, "wal"),
            small_bytes(5),
            Durability::Buffered,
        );
    }
    put(
        &mut db,
        "plain",
        Document::new("last"),
        small_bytes(50),
        Durability::Fsync,
    );
    drop(db);
    docs
}

/// The application's read-only open (with `tags@1`) shows exactly `docs`, each blob read whole and byte-identical.
fn assert_app_reads(path: &Path, docs: &[(&str, String, Vec<u8>)], ctx: &str) {
    let ro = ReadOnlyBlobDatabase::open(path, &tags_options()).unwrap();
    let snapshot = ro.database().snapshot();
    for collection in ["notes", "plain"] {
        let c = snapshot.collection(collection).unwrap();
        let expected: BTreeSet<&str> = docs
            .iter()
            .filter(|(name, ..)| *name == collection)
            .map(|(_, id, _)| id.as_str())
            .collect();
        let mut live = BTreeSet::new();
        for (_, id, bytes) in docs.iter().filter(|(name, ..)| *name == collection) {
            let doc = c
                .get(id)
                .unwrap()
                .unwrap_or_else(|| panic!("{ctx}: {collection}/{id} is gone"));
            let blob = doc.blob_ref().unwrap().clone();
            assert!(
                ro.get_blob_range(&blob, 0..blob.size()).unwrap() == *bytes,
                "{ctx}: {collection}/{id}: other bytes"
            );
            live.insert(id.as_str());
        }
        assert_eq!(live, expected, "{ctx}");
        assert!(
            c.get("g1").unwrap().is_none(),
            "{ctx}: a deleted note came back"
        );
    }
}

/// ROADMAP 10.4.8, in both orders (the `notes` record before or after a `plain` backlog that a writer's replay
/// flushes): `REFUSALS` runs of `persia gc` and of `persia compact` in a row each exit 1 and leave the container and
/// the pack byte-identical, so they add no segment, duplicate or not; the application's next open shows every document
/// and blob; once it has closed the database, both commands succeed, `persia gc` dropping exactly the three orphans.
#[test]
fn repeated_refused_gc_and_compact_runs_write_nothing_in_either_order() {
    const REFUSALS: usize = 3;
    let _serial = serial();
    for order in [Order::Before, Order::After] {
        let ctx = format!("{order:?}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let docs = with_a_backlog_and_a_note(&path, order);
        let before = (
            std::fs::read(&path).unwrap(),
            std::fs::read(pack(&path)).unwrap(),
        );
        for n in 0..REFUSALS {
            assert_gc_refused(&path, &format!("{ctx}, gc {n}"));
            assert_compact_refused(&path, &format!("{ctx}, compact {n}"));
        }
        let after = (
            std::fs::read(&path).unwrap(),
            std::fs::read(pack(&path)).unwrap(),
        );
        assert!(after == before, "{ctx}: the refusals changed a byte");
        assert_app_reads(&path, &docs, &format!("{ctx}: after the refusals"));

        close_with_the_application(&path);
        let done = gc_json(&path);
        assert_eq!(done["deleted"]["blobs"], 3, "{ctx}: {done}");
        assert_eq!(done["missing"], serde_json::json!([]), "{ctx}: {done}");
        assert_eq!(done["marked"]["blobs"], docs.len(), "{ctx}: {done}");
        let compacted = json(&persia(&["compact", arg(&path), "--json"]));
        assert_eq!(compacted["after"]["dead_bytes"], 0, "{ctx}: {compacted}");
        assert_app_reads(&path, &docs, &format!("{ctx}: after gc and compact"));
        let verified = persia(&["verify", "--deep", arg(&path)]);
        assert!(verified.status.success(), "{ctx}: {}", stderr(&verified));
    }
}

/// Review item M1: a refused grace costs no open. The database's WAL holds a backlog over the default 64 MiB memtable,
/// which a writer's open flushes into a commit (SPEC §8.6), and its pack's lock file is gone (a lock file is never
/// needed beforehand): `--grace 1h` exits 1 with the container and the pack byte-identical and no lock file created,
/// so the GC's own check (`GcOptions::validate`) ran before anything was opened.
#[test]
fn a_refused_grace_opens_nothing() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    with_orphans(&path);
    std::fs::remove_file(dir.path().join("app.persia.blobs.lock")).unwrap();
    let mut options = DatabaseOptions::default();
    options.policy.flush.memtable_bytes = 1 << 30;
    let mut db = Database::open(&path, options).unwrap();
    db.create_collection(SchemaBuilder::new("pad").strict(false).build().unwrap())
        .unwrap();
    // 18 documents of 4 MiB each (an extra stored field): 72 MiB, all in the WAL.
    let pad = Json::String("p".repeat(4 << 20));
    for i in 0..18 {
        let mut doc = Document::new(format!("big{i:02}"));
        doc.insert_extra("pad", pad.clone());
        db.put("pad", &doc, Durability::Buffered).unwrap();
    }
    db.put("pad", &Document::new("last"), Durability::Fsync)
        .unwrap();
    drop(db);
    let listing = || -> BTreeSet<PathBuf> {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect()
    };
    let before = (
        std::fs::read(&path).unwrap(),
        std::fs::read(pack(&path)).unwrap(),
        listing(),
    );
    let out = persia(&["gc", arg(&path), "--grace", "1h", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("twice the longest bind budget"),
        "{}",
        stderr(&out)
    );
    assert_eq!(out.stdout, b"");
    let after = (
        std::fs::read(&path).unwrap(),
        std::fs::read(pack(&path)).unwrap(),
        listing(),
    );
    assert!(after.0 == before.0, "the container is unchanged");
    assert!(after.1 == before.1, "the pack is unchanged");
    assert_eq!(after.2, before.2, "no lock file was created");
    // The same database with an accepted grace: the open flushes the backlog (a commit), and the GC runs.
    let done = gc_json(&path);
    assert_eq!(done["deleted"]["blobs"], 3, "{done}");
    assert!(
        std::fs::read(&path).unwrap() != before.0,
        "this open committed"
    );
}

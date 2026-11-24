//! The `persia` binary's database commands, end to end (ROADMAP 10.4.1): `stats`, `compact` and `dump documents`,
//! their output and `--json` shapes, and the exit status of every command that takes a container, for a sound
//! database, a missing path, a file that is not a container, a damaged one and one a live writer holds (the crate
//! docs of `persia-cli` hold the same table).

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::panic
)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use persia::internal::documents::{Blob, BlobDatabase};
use persia_engine::analysis::{Analyzer, AnalyzerRegistry, WhitespaceTokenizer};
use persia_engine::db::{Database, DatabaseOptions, ReadOnlyDatabase};
use persia_engine::document::{Document, Value};
use persia_engine::manifest::Location;
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;
use serde_json::Value as Json;

/// Serializes the tests of this binary. A test that spawns `persia` forks, and a forked child holds copies of every
/// descriptor the parent had open until it execs, flock included: a container another test had open at that moment
/// would then look locked, to that test and to the `persia` it runs.
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

fn path_arg(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The one JSON object a `--json` command wrote, which must have succeeded: one line, nothing else.
fn json(out: &Output) -> Json {
    let text = stdout(out);
    assert_eq!(text.lines().count(), 1, "{text}");
    serde_json::from_str(&text).unwrap()
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

/// `key value` rows of a text report, up to its first empty line.
fn rows(text: &str) -> Vec<(String, String)> {
    text.lines()
        .take_while(|l| !l.is_empty())
        .map(|l| {
            let (key, value) = l.split_at(18.min(l.len()));
            (key.trim_end().to_owned(), value.to_owned())
        })
        .collect()
}

fn row<'a>(rows: &'a [(String, String)], key: &str) -> &'a str {
    &rows
        .iter()
        .find(|(k, _)| k == key)
        .unwrap_or_else(|| panic!("no row {key:?} in {rows:?}"))
        .1
}

fn notes_schema() -> CollectionSchema {
    SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

fn note(schema: &CollectionSchema, id: &str, text: &str) -> Document {
    let body = schema.field("body").unwrap().id();
    Document::new(id).with(body, Value::String(text.to_owned()))
}

/// A database with one committed document, one synced but uncommitted, and its writer, still open.
fn database(path: &Path) -> Database {
    let schema = notes_schema();
    let mut db = Database::create(path, DatabaseOptions::default()).unwrap();
    db.create_collection(schema.clone()).unwrap();
    db.put("notes", &note(&schema, "n1", "note n1"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    db.put("notes", &note(&schema, "n2", "note n2"), Durability::Fsync)
        .unwrap();
    db
}

/// A database with two segments, a committed deletion, and a replacement and a new document in the WAL only; its
/// writer, still open, and the documents a reader sees, in storage order.
///
/// Segment 1 holds `a`, `b`, `c`; segment 2 holds `d`, and its commit deletes `b`. The WAL then replaces `a` and adds
/// `e`. So the live documents are `c` (segment 1), `d` (segment 2), then `a` and `e` (the WAL).
fn layered(path: &Path) -> (Database, Vec<Document>) {
    let schema = notes_schema();
    let mut db = Database::create(path, DatabaseOptions::default()).unwrap();
    db.create_collection(schema.clone()).unwrap();
    for id in ["a", "b", "c"] {
        db.put("notes", &note(&schema, id, "first"), Durability::Fsync)
            .unwrap();
    }
    db.commit().unwrap();
    db.put("notes", &note(&schema, "d", "second"), Durability::Fsync)
        .unwrap();
    db.delete("notes", "b", Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put(
        "notes",
        &note(&schema, "a", "replaced\nin the WAL"),
        Durability::Fsync,
    )
    .unwrap();
    db.put("notes", &note(&schema, "e", "third"), Durability::Fsync)
        .unwrap();
    let live = vec![
        note(&schema, "c", "first"),
        note(&schema, "d", "second"),
        note(&schema, "a", "replaced\nin the WAL"),
        note(&schema, "e", "third"),
    ];
    (db, live)
}

/// A closed database whose container holds dead frames: the same document committed `commits` times.
fn rewritten(path: &Path, commits: usize) {
    let schema = notes_schema();
    let mut db = Database::create(path, DatabaseOptions::default()).unwrap();
    db.create_collection(schema.clone()).unwrap();
    for n in 0..commits {
        let text = format!("version {n} {}", "padding ".repeat(64));
        db.put("notes", &note(&schema, "n1", &text), Durability::Fsync)
            .unwrap();
        db.commit().unwrap();
    }
}

fn read_only(path: &Path) -> ReadOnlyDatabase {
    ReadOnlyDatabase::open(path, &AnalyzerRegistry::builtin()).unwrap()
}

fn ids(path: &Path) -> Vec<String> {
    let out = persia(&["dump", "documents", path_arg(path), "--collection", "notes"]);
    stdout(&out)
        .lines()
        .map(|l| {
            let doc: Json = serde_json::from_str(l).unwrap();
            doc["id"].as_str().unwrap().to_owned()
        })
        .collect()
}

#[test]
fn stats_json_reports_a_database_beside_its_writer_in_its_documented_shape() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = database(&path);
    let before = std::fs::read(&path).unwrap();
    let stats = json(&persia(&["stats", path_arg(&path), "--json"]));
    // Read-only: the writer keeps its lock, the file is unchanged and nothing is created beside it.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    // The documented shape: these keys and no others.
    assert_eq!(
        keys(&stats),
        set(&[
            "path",
            "format",
            "db_uuid",
            "seq",
            "container",
            "wal",
            "collections",
            "blob_pack"
        ])
    );
    assert_eq!(keys(&stats["format"]), set(&["major", "minor"]));
    assert_eq!(
        keys(&stats["container"]),
        set(&[
            "bytes",
            "live_bytes",
            "dead_bytes",
            "compaction_due",
            "uncommitted_frames",
            "uncommitted_bytes"
        ])
    );
    assert_eq!(
        keys(&stats["wal"]),
        set(&[
            "wal_start",
            "log_end",
            "replayed_records",
            "frames_after_commit",
            "bytes_after_commit"
        ])
    );
    assert_eq!(keys(&stats["blob_pack"]), set(&["present", "bytes"]));
    let collection = &stats["collections"][0];
    assert_eq!(
        keys(collection),
        set(&[
            "name",
            "schema",
            "documents",
            "segment_documents",
            "deleted",
            "wal_documents",
            "segment_bytes",
            "segments"
        ])
    );
    assert_eq!(
        keys(&collection["schema"]),
        set(&["hash", "strict", "fields"])
    );
    assert_eq!(
        keys(&collection["segments"][0]),
        set(&["id", "documents", "deleted", "bytes"])
    );

    // The values, against what the engine and the container report.
    let container = writer.container();
    let superblock = container.superblock();
    let canonical = std::fs::canonicalize(&path).unwrap();
    assert_eq!(stats["path"], canonical.to_str().unwrap());
    assert_eq!(stats["format"], serde_json::json!({"major": 1, "minor": 0}));
    let uuid = format!("{:032x}", u128::from_be_bytes(container.header().db_uuid));
    assert_eq!(stats["db_uuid"], uuid);
    assert_eq!(stats["seq"], 3);
    assert_eq!(stats["wal"]["wal_start"], superblock.wal_start);
    assert_eq!(stats["wal"]["log_end"], superblock.log_end);
    assert_eq!(stats["wal"]["replayed_records"], 1);
    assert_eq!(stats["wal"]["frames_after_commit"], 1);
    let chunk = container.end() - superblock.log_end;
    assert_eq!(stats["wal"]["bytes_after_commit"], chunk);
    assert_eq!(stats["container"]["uncommitted_frames"], 1);
    assert_eq!(stats["container"]["uncommitted_bytes"], chunk);
    assert_eq!(stats["container"]["bytes"], container.end());
    let usage = writer.space_usage().unwrap();
    assert_eq!(stats["container"]["live_bytes"], usage.live);
    assert_eq!(stats["container"]["dead_bytes"], usage.dead());
    assert_eq!(stats["container"]["compaction_due"], false);
    assert_eq!(
        stats["blob_pack"],
        serde_json::json!({"present": false, "bytes": null})
    );

    assert_eq!(stats["collections"].as_array().unwrap().len(), 1);
    assert_eq!(collection["name"], "notes");
    let schema = notes_schema();
    assert_eq!(collection["schema"]["hash"], schema.hash().to_string());
    assert_eq!(collection["schema"]["strict"], true);
    assert_eq!(
        collection["schema"]["fields"],
        serde_json::json!([{"name": "body", "type": "text"}])
    );
    assert_eq!(collection["documents"], 2);
    assert_eq!(collection["segment_documents"], 1);
    assert_eq!(collection["deleted"], 0);
    assert_eq!(collection["wal_documents"], 1);
    let entry = writer.manifest().collection("notes").unwrap().segments()[0];
    assert_eq!(collection["segment_bytes"], entry.size);
    assert_eq!(
        collection["segments"],
        serde_json::json!([{"id": entry.id.to_string(), "documents": 1, "deleted": 0, "bytes": entry.size}])
    );
    drop(writer);
}

#[test]
fn stats_counts_what_tombstones_delete_and_what_the_wal_replaces() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let (writer, live) = layered(&path);
    let stats = json(&persia(&["stats", "--json", path_arg(&path)]));
    let collection = &stats["collections"][0];
    // `c`, `d`, and the WAL's `a` and `e`: the committed `a` is hidden by its replacement, `b` is deleted.
    assert_eq!(collection["documents"], live.len());
    assert_eq!(collection["segment_documents"], 4);
    assert_eq!(collection["deleted"], 1);
    assert_eq!(collection["wal_documents"], 2);
    let segments = collection["segments"].as_array().unwrap();
    let entries = writer.manifest().collection("notes").unwrap().segments();
    assert_eq!(segments.len(), 2);
    // Oldest first, as the manifest lists them: the first one holds the deletion.
    assert_eq!(segments[0]["id"], entries[0].id.to_string());
    assert_eq!(segments[0]["documents"], 3);
    assert_eq!(segments[0]["deleted"], 1);
    assert_eq!(segments[1]["id"], entries[1].id.to_string());
    assert_eq!(segments[1]["documents"], 1);
    assert_eq!(segments[1]["deleted"], 0);
    assert_eq!(
        collection["segment_bytes"],
        entries[0].size + entries[1].size
    );
    assert_eq!(stats["wal"]["replayed_records"], 2);
    drop(writer);
}

#[test]
fn stats_text_has_a_row_per_number_and_a_block_per_collection() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let (writer, _) = layered(&path);
    let usage = writer.space_usage().unwrap();
    let entries = writer
        .manifest()
        .collection("notes")
        .unwrap()
        .segments()
        .to_vec();
    drop(writer);
    let text = stdout(&persia(&["stats", path_arg(&path)]));
    let top = rows(&text);
    let found: Vec<&str> = top.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        found,
        [
            "path",
            "format",
            "db_uuid",
            "seq",
            "size",
            "compaction",
            "uncommitted",
            "wal",
            "blob pack",
            "collections"
        ]
    );
    assert_eq!(row(&top, "format"), "1.0");
    assert_eq!(row(&top, "seq"), "4");
    let percent = usage.dead() * 100 / usage.total;
    assert_eq!(
        row(&top, "size"),
        format!(
            "{} bytes ({} live, {} dead, {percent}%)",
            usage.total,
            usage.live,
            usage.dead()
        )
    );
    assert_eq!(
        row(&top, "compaction"),
        "not due with the default thresholds (40% and 1048576 bytes dead)"
    );
    assert_eq!(row(&top, "blob pack"), "none");
    assert_eq!(row(&top, "collections"), "1");
    assert!(
        row(&top, "wal").starts_with("2 records replayed, 2 frames ("),
        "{text}"
    );
    let block: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.starts_with("collection "))
        .collect();
    assert_eq!(block[0], "collection \"notes\"");
    assert!(
        block[1].starts_with("  schema          1 fields (\"body\" text), strict, hash "),
        "{text}"
    );
    assert_eq!(
        block[2],
        "  documents       4 live (2 in the WAL), 1 of 4 deleted in segments"
    );
    assert_eq!(
        block[3],
        format!(
            "  segments        2 ({} bytes)",
            entries[0].size + entries[1].size
        )
    );
    let segment = |line: &str| {
        line.split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        segment(block[4]),
        ["segment", "documents", "deleted", "bytes"]
    );
    assert_eq!(
        segment(block[5]),
        [
            entries[0].id.to_string(),
            "3".into(),
            "1".into(),
            entries[0].size.to_string()
        ]
    );
    assert_eq!(
        segment(block[6]),
        [
            entries[1].id.to_string(),
            "1".into(),
            "0".into(),
            entries[1].size.to_string()
        ]
    );
    assert_eq!(block.len(), 7);
}

#[test]
fn stats_reports_the_blob_pack_and_takes_no_reader_lease() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut writer = BlobDatabase::create(&path, DatabaseOptions::default()).unwrap();
    writer
        .log_mut()
        .create_collection(SchemaBuilder::new("files").build().unwrap())
        .unwrap();
    writer
        .put(
            "files",
            &Document::new("f1"),
            Blob::from_reader(&b"some blob bytes"[..]),
            Durability::Fsync,
        )
        .unwrap();
    writer.commit().unwrap();
    drop(writer);
    let pack = dir.path().join("app.persia.blobs");
    let stats = json(&persia(&["stats", path_arg(&path), "--json"]));
    assert_eq!(
        stats["blob_pack"],
        serde_json::json!({"present": true, "bytes": std::fs::metadata(&pack).unwrap().len()})
    );
    // It reads no blob, so it takes no lease: nothing is created beside the pack.
    assert!(!dir.path().join("app.persia.blobs.leases").exists());
    let text = stdout(&persia(&["stats", path_arg(&path)]));
    assert_eq!(
        row(&rows(&text), "blob pack"),
        format!("{} bytes", std::fs::metadata(&pack).unwrap().len())
    );
}

#[test]
fn stats_and_dump_read_a_schema_naming_a_registered_analyzer() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    registered_analyzer_database(&path);
    let stats = json(&persia(&["stats", path_arg(&path), "--json"]));
    assert_eq!(stats["collections"][0]["documents"], 2);
    assert_eq!(ids(&path), ["n1", "n2"]);
}

/// A closed database whose schema names an analyzer the `persia` binary does not have, with one committed document
/// and one in the WAL.
fn registered_analyzer_database(path: &Path) {
    let mut registry = AnalyzerRegistry::builtin();
    registry
        .register("tags", 1, Analyzer::builder(WhitespaceTokenizer).build())
        .unwrap();
    let schema = CollectionSchema::from_json_in(
        r#"{"name": "notes", "fields": {"tags": {"type": "text", "analyzer": "tags"}}}"#,
        &registry,
    )
    .unwrap();
    let tags = schema.field("tags").unwrap().id();
    let options = DatabaseOptions {
        registry,
        ..DatabaseOptions::default()
    };
    let mut db = Database::create(path, options).unwrap();
    db.create_collection(schema).unwrap();
    for id in ["n1", "n2"] {
        let doc = Document::new(id).with(tags, Value::String("a b".into()));
        db.put("notes", &doc, Durability::Fsync).unwrap();
        if id == "n1" {
            db.commit().unwrap();
        }
    }
}

#[test]
fn dump_documents_writes_each_live_document_once_in_storage_order() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let (writer, live) = layered(&path);
    let schema = notes_schema();
    let expected: Vec<String> = live.iter().map(|d| d.to_json(&schema).unwrap()).collect();
    let before = std::fs::read(&path).unwrap();
    let args = [
        "dump",
        "documents",
        path_arg(&path),
        "--collection",
        "notes",
    ];
    // Beside the live writer, and the same lines on every run.
    let text = stdout(&persia(&args));
    assert_eq!(text.lines().collect::<Vec<_>>(), expected);
    assert_eq!(stdout(&persia(&args)), text);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    // The SPEC §6.4.1 output form: `id` first, then the fields; a line break inside a value stays escaped.
    assert_eq!(
        text.lines().nth(2).unwrap(),
        r#"{"id":"a","body":"replaced\nin the WAL"}"#
    );
    assert!(text.ends_with("}\n"));
    // Each line reads back as the document it was.
    for (line, doc) in text.lines().zip(&live) {
        assert_eq!(&Document::from_json(line, &schema).unwrap(), doc);
    }
    drop(writer);
    // Closed, a writer's next commit moves the WAL's documents into a segment: the same set, in the new storage order.
    let mut db = Database::open(&path, DatabaseOptions::default()).unwrap();
    db.commit().unwrap();
    drop(db);
    assert_eq!(ids(&path), ["c", "d", "a", "e"]);
}

#[test]
fn dump_documents_of_an_empty_collection_writes_nothing() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    db.create_collection(notes_schema()).unwrap();
    drop(db);
    let out = persia(&[
        "dump",
        "documents",
        path_arg(&path),
        "--collection",
        "notes",
    ]);
    assert_eq!(stdout(&out), "");
}

#[test]
fn dump_documents_of_an_unknown_collection_names_the_ones_there_are() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(database(&path));
    let out = persia(&["dump", "documents", path_arg(&path), "--collection", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    let err = stderr(&out);
    assert!(
        err.contains("has no collection \"nope\" (it has: \"notes\")"),
        "{err}"
    );

    let empty = dir.path().join("empty.persia");
    drop(Database::create(&empty, DatabaseOptions::default()).unwrap());
    let out = persia(&[
        "dump",
        "documents",
        path_arg(&empty),
        "--collection",
        "nope",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("(it has: none)"), "{}", stderr(&out));
}

/// `persia dump documents ... | head -1`: the reader closes the pipe after the first line, long before the dump ends.
/// The command stops without a panic and without an error (as `dump segment` does).
#[test]
fn dump_documents_into_a_closed_pipe_stops_quietly() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let schema = notes_schema();
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    db.create_collection(schema.clone()).unwrap();
    // Far more output than a pipe buffers (64 KiB on Linux), in a segment and in the WAL.
    let text = "a note long enough to fill a pipe ".repeat(8);
    let docs: Vec<Document> = (0..4000)
        .map(|n| note(&schema, &format!("n{n:05}"), &text))
        .collect();
    db.put_batch("notes", &docs[..3000], Durability::Buffered)
        .unwrap();
    db.commit().unwrap();
    db.put_batch("notes", &docs[3000..], Durability::Fsync)
        .unwrap();
    drop(db);

    let mut child = Command::new(env!("CARGO_BIN_EXE_persia"))
        .args([
            "dump",
            "documents",
            path_arg(&path),
            "--collection",
            "notes",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::with_capacity(512, child.stdout.take().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    assert!(first.starts_with(r#"{"id":"n00000","body":"#), "{first}");
    drop(reader);
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(err, "");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn compact_reclaims_dead_frames_and_keeps_every_document() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    rewritten(&path, 6);
    // A record in the WAL only: the compaction commits no document, and must not lose it.
    let schema = notes_schema();
    let mut db = Database::open(&path, DatabaseOptions::default()).unwrap();
    db.put(
        "notes",
        &note(&schema, "n2", "in the WAL"),
        Durability::Fsync,
    )
    .unwrap();
    let before = db.space_usage().unwrap();
    let seq = db.seq();
    drop(db);
    assert!(before.dead() > 0);
    let size_before = std::fs::metadata(&path).unwrap().len();

    let text = stdout(&persia(&["compact", path_arg(&path)]));
    let found = rows(&text);
    let after = Database::open(&path, DatabaseOptions::default()).unwrap();
    let now = after.space_usage().unwrap();
    assert_eq!(
        found,
        [
            ("seq".to_owned(), (seq + 1).to_string()),
            (
                "before".to_owned(),
                format!(
                    "{} bytes ({} live, {} dead)",
                    before.total,
                    before.live,
                    before.dead()
                )
            ),
            (
                "after".to_owned(),
                format!("{} bytes ({} live, 0 dead)", now.total, now.live)
            ),
            (
                "reclaimed".to_owned(),
                format!("{} bytes", before.total - now.total)
            ),
        ]
    );
    assert_eq!(now.dead(), 0);
    assert_eq!(after.seq(), seq + 1);
    assert!(std::fs::metadata(&path).unwrap().len() < size_before);
    // Still one committed document and one replayed from the WAL, which stayed uncommitted.
    let snapshot = after.snapshot();
    let notes = snapshot.collection("notes").unwrap();
    assert!(notes.get("n1").unwrap().is_some());
    assert_eq!(
        notes.get("n2").unwrap(),
        Some(note(&schema, "n2", "in the WAL"))
    );
    assert_eq!(notes.memtable().live_count(), 1);
    drop(after);
    // No temporary file is left, and the result verifies.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    assert!(persia(&["verify", path_arg(&path)]).status.success());
}

#[test]
fn compact_json_has_its_documented_shape() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    rewritten(&path, 4);
    let db = Database::open(&path, DatabaseOptions::default()).unwrap();
    let (before, seq) = (db.space_usage().unwrap(), db.seq());
    drop(db);
    let done = json(&persia(&["compact", "--json", path_arg(&path)]));
    assert_eq!(
        keys(&done),
        set(&["seq", "before", "after", "reclaimed_bytes"])
    );
    for side in ["before", "after"] {
        assert_eq!(
            keys(&done[side]),
            set(&["bytes", "live_bytes", "dead_bytes"])
        );
    }
    let now = read_only(&path).container().end();
    assert_eq!(done["seq"], seq + 1);
    assert_eq!(
        done["before"],
        serde_json::json!({"bytes": before.total, "live_bytes": before.live, "dead_bytes": before.dead()})
    );
    assert_eq!(
        done["after"],
        serde_json::json!({"bytes": now, "live_bytes": now, "dead_bytes": 0})
    );
    assert_eq!(done["reclaimed_bytes"], before.total - now);
    // `stats` agrees with it.
    let stats = json(&persia(&["stats", path_arg(&path), "--json"]));
    assert_eq!(stats["seq"], seq + 1);
    assert_eq!(stats["container"]["bytes"], now);
    assert_eq!(stats["container"]["dead_bytes"], 0);
}

/// A container whose first commit never completed (created, superblock `seq` 0; SPEC §8.6 reads it as the empty
/// manifest): `seq` 0, no collection, nothing dead, no pack. A database's create commits once, so it reads `seq` 1.
#[test]
fn stats_of_a_container_without_a_commit_reports_seq_0() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(persia_storage::container::Container::create(&path).unwrap());
    let stats = json(&persia(&["stats", path_arg(&path), "--json"]));
    assert_eq!(stats["seq"], 0, "{stats}");
    assert_eq!(stats["collections"], serde_json::json!([]));
    assert_eq!(stats["container"]["dead_bytes"], 0);
    assert_eq!(stats["container"]["compaction_due"], false);
    assert_eq!(stats["wal"]["replayed_records"], 0);
    assert_eq!(stats["blob_pack"]["present"], false);
    let text = stdout(&persia(&["stats", path_arg(&path)]));
    assert_eq!(row(&rows(&text), "seq"), "0");

    let created = dir.path().join("created.persia");
    drop(Database::create(&created, DatabaseOptions::default()).unwrap());
    let stats = json(&persia(&["stats", path_arg(&created), "--json"]));
    assert_eq!(stats["seq"], 1, "{stats}");
    assert_eq!(stats["collections"], serde_json::json!([]));
}

/// A crash during a compaction leaves the old file and `<name>.compact.*` beside it (SPEC §4.6): `container sweep`
/// removes that file, and the database still compacts.
#[test]
fn container_sweep_removes_what_a_crashed_compaction_left() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("app.persia");
    rewritten(&path, 3);
    let leftover = root.join("app.persia.compact.0123456789abcdef.0");
    std::fs::write(&leftover, b"half a compacted container").unwrap();
    let swept = stdout(&persia(&["container", "sweep", path_arg(&path)]));
    assert_eq!(
        swept,
        format!("removed\ttemporary\tfree\t{}\n", leftover.display())
    );
    assert!(!leftover.exists());
    assert!(persia(&["compact", path_arg(&path)]).status.success());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    assert!(persia(&["verify", path_arg(&path)]).status.success());
}

/// `compact` commits no document of its own, but its open is a writer's open: a WAL backlog larger than the default
/// memtable (64 MiB) is flushed into a commit by recovery (SPEC §8.6), so `seq` rises by more than one and the backlog
/// lands in a segment. The application that logged it had a larger memtable, which the command cannot know.
#[test]
fn compact_after_a_wal_backlog_over_the_default_memtable_commits_it_in_recovery() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let schema = SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .strict(false)
        .build()
        .unwrap();
    let mut options = DatabaseOptions::default();
    options.policy.flush.memtable_bytes = 1 << 30;
    let mut db = Database::create(&path, options).unwrap();
    db.create_collection(schema.clone()).unwrap();
    db.commit().unwrap();
    let seq = db.seq();
    // 18 documents of 4 MiB each (an extra field, stored and not analyzed): 72 MiB, all in the WAL.
    let pad = serde_json::Value::String("p".repeat(4 << 20));
    for i in 0..18 {
        let mut doc = note(&schema, &format!("big{i:02}"), "padded");
        doc.insert_extra("pad", pad.clone());
        db.put("notes", &doc, Durability::Buffered).unwrap();
    }
    db.put("notes", &note(&schema, "last", "synced"), Durability::Fsync)
        .unwrap();
    assert_eq!(db.seq(), seq);
    drop(db);

    let done = json(&persia(&["compact", "--json", path_arg(&path)]));
    let compacted = done["seq"].as_u64().unwrap();
    assert!(compacted >= seq + 2, "{done} after seq {seq}");
    let after = read_only(&path);
    assert_eq!(after.seq(), compacted);
    let snapshot = after.snapshot();
    let notes = snapshot.collection("notes").unwrap();
    let in_segments: u64 = notes
        .segments()
        .iter()
        .map(|s| u64::from(s.doc_count()))
        .sum();
    assert!(in_segments > 0, "the backlog was flushed into a segment");
    assert_eq!(in_segments + u64::from(notes.memtable().live_count()), 19);
    assert!(notes.get("last").unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn compact_fails_while_a_writer_holds_the_database() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = database(&path);
    let before = std::fs::read(&path).unwrap();
    let out = persia(&["compact", path_arg(&path)]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert!(
        stderr(&out).contains("is open by a writer; compact it once that is closed"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    drop(writer);
}

/// SPEC §4.6 rewrites the container only: the blob pack keeps its bytes, and every reference still finds its blob.
#[test]
fn compact_leaves_the_blob_pack_alone() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut writer = BlobDatabase::create(&path, DatabaseOptions::default()).unwrap();
    writer
        .log_mut()
        .create_collection(SchemaBuilder::new("files").build().unwrap())
        .unwrap();
    for round in 0..3 {
        writer
            .put(
                "files",
                &Document::new("f1"),
                Blob::from_reader(format!("blob {round}").as_bytes()),
                Durability::Fsync,
            )
            .unwrap();
        writer.commit().unwrap();
    }
    drop(writer);
    let pack = dir.path().join("app.persia.blobs");
    let before = std::fs::read(&pack).unwrap();
    let done = json(&persia(&["compact", path_arg(&path), "--json"]));
    assert!(done["reclaimed_bytes"].as_u64().unwrap() > 0, "{done}");
    assert_eq!(std::fs::read(&pack).unwrap(), before);
    assert!(!dir.path().join("app.persia.blobs.leases").exists());
    let verified = persia(&["verify", "--deep", path_arg(&path)]);
    assert!(verified.status.success(), "{}", stderr(&verified));
}

/// The command has the built-in analyzers only, and a read-write open needs every analyzer the schemas name
/// (SPEC §6.2.2): it fails as any other failure (status 1), and leaves the file as it was.
#[test]
fn compact_refuses_a_schema_naming_a_registered_analyzer() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    registered_analyzer_database(&path);
    let before = std::fs::read(&path).unwrap();
    let out = persia(&["compact", path_arg(&path)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("tags@1"), "{}", stderr(&out));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// The situations of the exit status table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// A sound database, closed.
    Sound,
    /// No file at the path.
    Missing,
    /// A file that is not a Persia container.
    NotContainer,
    /// A container whose header fails its CRC (the magic is intact).
    Corrupt,
}

const STATES: [State; 4] = [
    State::Sound,
    State::Missing,
    State::NotContainer,
    State::Corrupt,
];

/// A path in `dir` in the state `state`.
fn in_state(dir: &Path, state: State) -> PathBuf {
    let path = dir.join("app.persia");
    match state {
        State::Sound => drop(database(&path)),
        State::Missing => {}
        State::NotContainer => std::fs::write(&path, b"this is not a database").unwrap(),
        State::Corrupt => {
            drop(database(&path));
            let mut bytes = std::fs::read(&path).unwrap();
            bytes[12] ^= 0xff;
            std::fs::write(&path, &bytes).unwrap();
        }
    }
    path
}

/// The arguments of every command that takes a container, by a name for messages; `{}` stands for the path.
const COMMANDS: [(&str, &[&str]); 10] = [
    ("verify", &["verify", "{}"]),
    ("verify --quick", &["verify", "--quick", "{}"]),
    ("stats", &["stats", "{}"]),
    ("stats --json", &["stats", "{}", "--json"]),
    (
        "dump documents",
        &["dump", "documents", "{}", "--collection", "notes"],
    ),
    ("inspect container", &["inspect", "container", "{}"]),
    ("compact", &["compact", "{}"]),
    ("compact --json", &["compact", "{}", "--json"]),
    ("container sweep", &["container", "sweep", "{}"]),
    ("container files", &["container", "files", "{}"]),
];

fn run_on(args: &[&str], path: &Path) -> Output {
    let args: Vec<&str> = args
        .iter()
        .map(|a| if *a == "{}" { path_arg(path) } else { a })
        .collect();
    persia(&args)
}

/// The exit status of each command in each state (the crate docs' table).
fn expected_status(command: &str, state: State) -> i32 {
    // `container files` opens nothing: it lists the files beside any existing file, whatever that file holds.
    if command == "container files" {
        return i32::from(state == State::Missing);
    }
    match state {
        State::Sound => 0,
        State::Missing => 1,
        State::NotContainer => 4,
        State::Corrupt => 3,
    }
}

#[test]
fn every_command_exits_with_its_documented_status() {
    let _serial = serial();
    for (name, args) in COMMANDS {
        for state in STATES {
            let dir = tempfile::tempdir().unwrap();
            let path = in_state(dir.path(), state);
            let out = run_on(args, &path);
            assert_eq!(
                out.status.code(),
                Some(expected_status(name, state)),
                "{name} on {state:?}: {}",
                stderr(&out)
            );
            // A failure says why on stderr, and writes no report.
            if state != State::Sound && expected_status(name, state) != 0 {
                assert!(!stderr(&out).is_empty(), "{name} on {state:?}");
                assert_eq!(out.stdout, b"", "{name} on {state:?}");
            }
            // No command creates the file it was asked about, or leaves a file beside a path it could not open.
            if state == State::Missing {
                assert_eq!(
                    std::fs::read_dir(dir.path()).unwrap().count(),
                    0,
                    "{name} on {state:?}"
                );
            }
        }
    }
}

/// Beside a live writer: the read-only commands succeed, the two that open read-write fail with status 1 and say who
/// holds the database.
#[cfg(unix)]
#[test]
fn every_command_exits_with_its_documented_status_beside_a_live_writer() {
    let _serial = serial();
    for (name, args) in COMMANDS {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let writer = database(&path);
        let out = run_on(args, &path);
        let read_write = matches!(name, "compact" | "compact --json" | "container sweep");
        assert_eq!(
            out.status.code(),
            Some(i32::from(read_write)),
            "{name}: {}",
            stderr(&out)
        );
        if read_write {
            assert!(
                stderr(&out).contains("is open by a writer"),
                "{name}: {}",
                stderr(&out)
            );
        }
        drop(writer);
    }
}

/// Damage below `log_end`, which no recovery repairs: a flipped byte inside the committed segment's frame. Every
/// command that reads the documents fails with the corruption status, read-only or read-write.
#[test]
fn a_damaged_committed_segment_exits_with_3() {
    let _serial = serial();
    for args in [
        &["stats", "{}"][..],
        &["dump", "documents", "{}", "--collection", "notes"],
        &["compact", "{}"],
        &["verify", "{}"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        drop(database(&path));
        let entry = read_only(&path)
            .manifest()
            .collection("notes")
            .unwrap()
            .segments()[0];
        let Location::Frame { offset, len } = entry.location else {
            panic!("a local segment is a frame");
        };
        let mut bytes = std::fs::read(&path).unwrap();
        let at = usize::try_from(offset + len / 2).unwrap();
        bytes[at] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let out = run_on(args, &path);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", stderr(&out));
        assert_eq!(out.stdout, b"", "{args:?}");
        // A failed compaction leaves the damaged file as it is, for `verify` and a repair.
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{args:?}");
    }
}

#[test]
fn usage_errors_exit_with_2() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(database(&path));
    for args in [
        vec!["stats"],
        vec!["compact"],
        vec!["dump", "documents", path_arg(&path)],
        vec!["dump", "documents", "--collection", "notes"],
        vec!["stats", path_arg(&path), "--yaml"],
    ] {
        let out = persia(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
}

/// One tree: the groups and the top-level commands, each with a help text that states its exit statuses.
#[test]
fn help_lists_the_command_tree_and_each_commands_exit_statuses() {
    let _serial = serial();
    let root = stdout(&persia(&["--help"]));
    for command in [
        "container",
        "store",
        "inspect",
        "dump",
        "verify",
        "compact",
        "stats",
    ] {
        assert!(
            root.lines()
                .any(|l| l.trim_start().starts_with(&format!("{command} "))),
            "{command} in\n{root}"
        );
    }
    assert!(
        root.contains(
            "Exit status: 0 success; 1 another failure; 2 usage error; 3 corrupt Persia data or a missing referenced \
             blob; 4 the file is not a Persia container"
        ),
        "{root}"
    );
    let dump = stdout(&persia(&["dump", "--help"]));
    for command in ["segment", "documents"] {
        assert!(
            dump.lines()
                .any(|l| l.trim_start().starts_with(&format!("{command} "))),
            "{command} in\n{dump}"
        );
    }
    for args in [
        &["compact", "--help"][..],
        &["stats", "--help"],
        &["dump", "documents", "--help"],
    ] {
        let help = stdout(&persia(args));
        let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains(
                "status 3 on corruption, 4 when the file is not a Persia container, and 1 on any other failure"
            ),
            "{args:?}:\n{help}"
        );
        assert!(flat.contains("<PATH>"), "{args:?}:\n{help}");
    }
}

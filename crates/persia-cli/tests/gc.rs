//! `persia gc` end to end (ROADMAP 10.4.5; SPEC §9.5): orphan blobs deleted and the pack rewritten smaller, referenced
//! blobs kept and `verify --deep` clean after it, the local pack's lack of ages (no grace-young object kept), a reader
//! lease behind the committed seq holding the sweep back, a database without a pack, dangling references reported,
//! the `--grace` checks, the `--json` keys and the text rows, and a writer holding the database. The exit status of
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
use persia_engine::document::Document;
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

/// The command has the built-in analyzers only, and GC's writer open needs every analyzer the schemas name: it fails
/// (status 1), says so, and leaves the files as they were (ROADMAP 10.4.7).
#[test]
fn gc_refuses_a_schema_naming_a_registered_analyzer() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut registry = AnalyzerRegistry::builtin();
    registry
        .register("tags", 1, Analyzer::builder(WhitespaceTokenizer).build())
        .unwrap();
    let schema = CollectionSchema::from_json_in(
        r#"{"name": "notes", "fields": {"tags": {"type": "text", "analyzer": "tags"}}}"#,
        &registry,
    )
    .unwrap();
    let options = DatabaseOptions {
        registry,
        ..DatabaseOptions::default()
    };
    let mut db = BlobDatabase::create(&path, options).unwrap();
    db.log_mut().create_collection(schema).unwrap();
    let bytes = bytes_of(9);
    db.put(
        "notes",
        &Document::new("n"),
        Blob::from_reader(&bytes[..]),
        Durability::Fsync,
    )
    .unwrap();
    db.commit().unwrap();
    drop(db);
    let before = (
        std::fs::read(&path).unwrap(),
        std::fs::read(pack(&path)).unwrap(),
    );
    let out = persia(&["gc", arg(&path)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("tags@1"), "{err}");
    assert!(
        err.contains("persia gc has the built-in analyzers only"),
        "{err}"
    );
    let after = (
        std::fs::read(&path).unwrap(),
        std::fs::read(pack(&path)).unwrap(),
    );
    assert!(after == before, "the files are unchanged");
}

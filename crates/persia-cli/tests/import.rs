//! `persia import` and `persia search` end to end (ROADMAP 10.4.2): a sound import (a file and the standard input,
//! `--schema` creating the database and the collection), the round trip `dump documents` -> `import`, every kind of
//! bad line stopping the import at its 1-based number with the lines before it committed and nothing after, a
//! missing database or input, a database a live writer holds, and a `BlobRef` the target database's blob store does
//! not hold; then `search` by user text and by request JSON, beside a live writer, and its failures. The exit status
//! of both commands for a sound, missing, non-container and damaged file is in `tests/commands.rs`'s table.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::panic
)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use persia::internal::documents::{Blob, BlobDatabase};
use persia_engine::analysis::{Analyzer, AnalyzerRegistry, WhitespaceTokenizer};
use persia_engine::db::{Database, DatabaseOptions};
use persia_engine::document::Document;
use persia_engine::schema::{CollectionSchema, SchemaBuilder};
use persia_engine::wal::Durability;
use serde_json::Value as Json;

/// As in `tests/commands.rs`: a spawned `persia` must not inherit a container lock another test holds.
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

/// `persia` with `input` on its standard input.
fn persia_with_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_persia"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn arg(p: &Path) -> &str {
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

fn json(out: &Output) -> Json {
    let text = stdout(out);
    assert_eq!(text.lines().count(), 1, "{text}");
    serde_json::from_str(&text).unwrap()
}

const NOTES: &str = r#"{"name": "notes", "fields": {"body": {"type": "text"}, "n": {"type": "i64", "stored": true}}}"#;

fn notes_schema() -> CollectionSchema {
    CollectionSchema::from_json(NOTES).unwrap()
}

/// A schema file in `dir`.
fn schema_file(dir: &Path, json: &str) -> PathBuf {
    let path = dir.join("schema.json");
    std::fs::write(&path, json).unwrap();
    path
}

/// An input file in `dir`.
fn input_file(dir: &Path, bytes: &[u8]) -> PathBuf {
    let path = dir.join("input.jsonl");
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A closed database at `path` with the empty collection `notes`.
fn empty_notes(path: &Path) {
    let mut db = Database::create(path, DatabaseOptions::default()).unwrap();
    db.create_collection(notes_schema()).unwrap();
}

/// The `dump documents` lines of `collection`.
fn dump(path: &Path, collection: &str) -> Vec<String> {
    let out = persia(&["dump", "documents", arg(path), "--collection", collection]);
    stdout(&out).lines().map(str::to_owned).collect()
}

fn ids(path: &Path) -> Vec<String> {
    dump(path, "notes")
        .iter()
        .map(|line| {
            let doc: Json = serde_json::from_str(line).unwrap();
            doc["id"].as_str().unwrap().to_owned()
        })
        .collect()
}

fn import_args<'a>(path: &'a Path, input: &'a Path) -> Vec<&'a str> {
    vec![
        "import",
        arg(path),
        "--collection",
        "notes",
        "--input",
        arg(input),
    ]
}

#[test]
fn import_with_a_schema_creates_the_database_and_reports_what_it_put() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let schema = schema_file(dir.path(), NOTES);
    // `\r\n` endings, a blank line, a replaced id, and no ending on the last line.
    let input = input_file(
        dir.path(),
        b"{\"id\": \"a\", \"body\": \"first note\", \"n\": 1}\r\n\n  \t\n{\"id\": \"b\", \"body\": \"second\"}\n{\"id\": \"a\", \"body\": \"replaced\", \"n\": 2}",
    );
    let mut args = import_args(&path, &input);
    args.extend(["--schema", arg(&schema), "--json"]);
    let done = json(&persia(&args));
    assert_eq!(
        done,
        serde_json::json!({"collection": "notes", "created": true, "lines": 5, "blank_lines": 2, "documents": 3})
    );
    assert_eq!(
        dump(&path, "notes"),
        [
            r#"{"id":"b","body":"second"}"#,
            r#"{"id":"a","body":"replaced","n":2}"#
        ]
    );
    // Committed at the end: the documents are in a segment, not only in the log.
    let stats = json(&persia(&["stats", arg(&path), "--json"]));
    assert_eq!(stats["collections"][0]["segment_documents"], 2, "{stats}");
    assert_eq!(stats["collections"][0]["wal_documents"], 0, "{stats}");

    // Again, into the collection now there, with the same schema: nothing created, the documents replaced.
    let text = stdout(&persia(&args[..args.len() - 1]));
    assert_eq!(
        text,
        "collection        notes\nlines             5\nblank lines       2\ndocuments         3\n"
    );
    assert_eq!(ids(&path), ["b", "a"]);
}

#[test]
fn import_reads_the_standard_input_without_input() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    empty_notes(&path);
    let out = persia_with_stdin(
        &["import", arg(&path), "--collection", "notes", "--json"],
        b"{\"id\": \"s1\"}\n{\"id\": \"s2\", \"body\": \"from stdin\"}\n",
    );
    assert_eq!(json(&out)["documents"], 2);
    assert_eq!(ids(&path), ["s1", "s2"]);
    // An empty input imports nothing and succeeds.
    let out = persia_with_stdin(
        &["import", arg(&path), "--collection", "notes", "--json"],
        b"",
    );
    assert_eq!(
        json(&out),
        serde_json::json!({"collection": "notes", "created": false, "lines": 0, "blank_lines": 0, "documents": 0})
    );
}

/// What `dump documents` writes, `import` reads back: a database with segments, a deletion and WAL records, every
/// field type and extra fields, dumped, imported into a new database with the same schema, and dumped again, gives
/// the same lines in the same order (the import commits them in input order, into one segment).
#[test]
fn a_dump_imports_back_into_a_new_database_line_for_line() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.persia");
    let schema_json = r#"{"name": "all", "strict": false, "fields": {
        "title": {"type": "text"}, "tags": {"type": "keyword"}, "n": {"type": "i64", "stored": true},
        "x": {"type": "f64", "stored": true}, "ok": {"type": "bool", "stored": true},
        "at": {"type": "datetime", "stored": true}, "meta": {"type": "stored"}}}"#;
    let schema = CollectionSchema::from_json(schema_json).unwrap();
    let lines = [
        r#"{"id": "d1", "title": "two\nlines \"quoted\"  ", "tags": ["a", "b"], "n": -7, "x": 0.5}"#,
        r#"{"id": "d2", "ok": true, "at": "2024-02-29T12:00:00.123456Z", "meta": {"k": [1, null, "v"]}}"#,
        r#"{"id": "d3", "extra_field": {"nested": true}, "x": -0.0}"#,
        r#"{"id": "d4", "title": "deleted later"}"#,
        r#"{"id": "d5", "tags": "single", "n": 9223372036854775807}"#,
    ];
    let mut db = Database::create(&source, DatabaseOptions::default()).unwrap();
    db.create_collection(schema.clone()).unwrap();
    for (i, line) in lines.iter().enumerate() {
        db.put(
            "all",
            &Document::from_json(line, &schema).unwrap(),
            Durability::Fsync,
        )
        .unwrap();
        if i == 2 {
            db.commit().unwrap();
        }
    }
    db.delete("all", "d4", Durability::Fsync).unwrap();
    drop(db);
    let dumped = dump(&source, "all");
    assert_eq!(dumped.len(), 4);

    let target = dir.path().join("target.persia");
    let input = input_file(dir.path(), format!("{}\n", dumped.join("\n")).as_bytes());
    let schema_path = schema_file(dir.path(), schema_json);
    let out = persia(&[
        "import",
        arg(&target),
        "--collection",
        "all",
        "--input",
        arg(&input),
        "--schema",
        arg(&schema_path),
        "--json",
    ]);
    assert_eq!(json(&out)["documents"], 4);
    assert_eq!(dump(&target, "all"), dumped);
}

/// Every kind of bad line stops the import there: status 1, no report on stdout, the line named on stderr, the
/// documents of the lines before it committed, and nothing from it on.
#[test]
fn a_bad_line_stops_the_import_at_its_number_with_the_lines_before_it_committed() {
    let _serial = serial();
    let long_line = {
        let mut line = br#"{"id": "big", "body": ""#.to_vec();
        line.resize(16 << 20, b'x');
        line.extend_from_slice(b"\"}");
        line
    };
    let long_id = format!(r#"{{"id": "{}"}}"#, "i".repeat(513));
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "not JSON",
            b"{\"id\": \"x\", ".to_vec(),
            "not a document of the collection \"notes\"",
        ),
        (
            "not an object",
            b"[1, 2]".to_vec(),
            "not a document of the collection \"notes\"",
        ),
        (
            "no id",
            br#"{"body": "no id"}"#.to_vec(),
            "not a document of the collection \"notes\"",
        ),
        (
            "an unknown field",
            br#"{"id": "x", "colour": "red"}"#.to_vec(),
            "colour",
        ),
        (
            "a value of another type",
            br#"{"id": "x", "n": "seven"}"#.to_vec(),
            "field n: expected an i64",
        ),
        (
            "a duplicate key",
            br#"{"id": "x", "n": 1, "n": 2}"#.to_vec(),
            "not a document",
        ),
        (
            "an id over 512 bytes",
            long_id.into_bytes(),
            "not a document",
        ),
        (
            "invalid UTF-8",
            b"{\"id\": \"\xff\"}".to_vec(),
            "not valid UTF-8 (byte 8 of the line)",
        ),
        (
            "a line over 16 MiB",
            long_line,
            "longer than 16777216 bytes",
        ),
    ];
    for (kind, bad, reason) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        empty_notes(&path);
        let mut input = b"{\"id\": \"before1\"}\n\n{\"id\": \"before2\"}\n".to_vec();
        input.extend_from_slice(&bad);
        input.extend_from_slice(b"\n{\"id\": \"after\"}\n");
        let input = input_file(dir.path(), &input);
        let out = persia(&import_args(&path, &input));
        assert_eq!(out.status.code(), Some(1), "{kind}: {}", stderr(&out));
        assert_eq!(out.stdout, b"", "{kind}");
        let err = stderr(&out);
        assert!(err.contains("line 4: "), "{kind}: {err}");
        assert!(err.contains(reason), "{kind}: {err}");
        assert!(
            err.contains("documents put before it, imported: 2; nothing from that line on"),
            "{kind}: {err}"
        );
        assert_eq!(ids(&path), ["before1", "before2"], "{kind}");
    }
}

#[test]
fn import_fails_without_a_database_or_an_input_and_creates_nothing() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let input = input_file(dir.path(), b"{\"id\": \"a\"}\n");
    // No database, no --schema: nothing is created.
    let out = persia(&import_args(&path, &input));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("is not a container file"),
        "{}",
        stderr(&out)
    );
    assert!(!path.exists());
    // A missing input fails before the database is created, even with --schema.
    let schema = schema_file(dir.path(), NOTES);
    let missing = dir.path().join("missing.jsonl");
    let mut args = import_args(&path, &missing);
    args.extend(["--schema", arg(&schema)]);
    let out = persia(&args);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("opening the input"),
        "{}",
        stderr(&out)
    );
    assert!(!path.exists());
    // A bad schema file fails before the database is created too.
    let bad = schema_file(
        dir.path(),
        r#"{"name": "notes", "fields": {"body": {"type": "txt"}}}"#,
    );
    let mut args = import_args(&path, &input);
    args.extend(["--schema", arg(&bad)]);
    let out = persia(&args);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("reading the schema"),
        "{}",
        stderr(&out)
    );
    assert!(!path.exists());
    // A schema of another collection than --collection.
    let other = schema_file(dir.path(), r#"{"name": "other", "fields": {}}"#);
    let mut args = import_args(&path, &input);
    args.extend(["--schema", arg(&other)]);
    let out = persia(&args);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("is of the collection \"other\", not \"notes\""),
        "{}",
        stderr(&out)
    );
    assert!(!path.exists());
}

#[test]
fn import_into_a_collection_it_lacks_or_of_another_schema_fails() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    empty_notes(&path);
    let input = input_file(dir.path(), b"{\"id\": \"a\"}\n");
    let out = persia(&[
        "import",
        arg(&path),
        "--collection",
        "other",
        "--input",
        arg(&input),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "has no collection \"other\" (it has: \"notes\"); give --schema to create it"
        ),
        "{}",
        stderr(&out)
    );
    let changed = schema_file(
        dir.path(),
        r#"{"name": "notes", "fields": {"body": {"type": "keyword"}}}"#,
    );
    let mut args = import_args(&path, &input);
    args.extend(["--schema", arg(&changed)]);
    let out = persia(&args);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("has another schema"),
        "{}",
        stderr(&out)
    );
    assert_eq!(ids(&path), Vec::<String>::new());
}

#[cfg(unix)]
#[test]
fn import_fails_while_a_writer_holds_the_database() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut writer = Database::create(&path, DatabaseOptions::default()).unwrap();
    writer.create_collection(notes_schema()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let input = input_file(dir.path(), b"{\"id\": \"a\"}\n");
    let out = persia(&import_args(&path, &input));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert!(
        stderr(&out).contains("is open by a writer; import into it once that is closed"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(writer);
}

/// A database whose collection `files` has `f1` with a blob and `f2` without, closed; and the dump of `files`.
fn with_a_blob(path: &Path) -> Vec<String> {
    let mut db = BlobDatabase::create(path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new("files").build().unwrap())
        .unwrap();
    db.put(
        "files",
        &Document::new("f1"),
        Blob::from_reader(&b"the blob's bytes"[..]),
        Durability::Fsync,
    )
    .unwrap();
    db.log_mut()
        .put("files", &Document::new("f2"), Durability::Fsync)
        .unwrap();
    db.commit().unwrap();
    drop(db);
    dump(path, "files")
}

/// A dump carries blob references, not their bytes (ROADMAP 10.4.6): imported into another database, a line whose
/// `BlobRef` that database's blob store does not hold is refused, naming the line and the `blob_id`, and nothing is
/// logged for it (SPEC §9.6). Into the database that holds the blob, the same line imports.
#[test]
fn a_blob_reference_the_target_does_not_hold_stops_the_import_at_its_line() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.persia");
    let dumped = with_a_blob(&source);
    assert!(dumped[0].contains("\"blob\""), "{dumped:?}");
    let blob: Json = serde_json::from_str(&dumped[0]).unwrap();
    let blob_id = blob["blob"]["blob_id"].as_str().unwrap().to_owned();
    // The line without a blob first, so the one with it is line 2.
    let input = input_file(
        dir.path(),
        format!("{}\n{}\n", dumped[1], dumped[0]).as_bytes(),
    );
    let schema = schema_file(dir.path(), r#"{"name": "files", "fields": {}}"#);

    let target = dir.path().join("target.persia");
    let out = persia(&[
        "import",
        arg(&target),
        "--collection",
        "files",
        "--input",
        arg(&input),
        "--schema",
        arg(&schema),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains(&format!(
            "line 2: the document \"f1\" references the blob {blob_id}, which is not in this database's blob store"
        )),
        "{err}"
    );
    assert!(
        err.contains("documents put before it, imported: 1;"),
        "{err}"
    );
    let imported: Vec<String> = dump(&target, "files");
    assert_eq!(imported, [dumped[1].clone()]);
    let verified = persia(&["verify", arg(&target)]);
    assert!(verified.status.success(), "{}", stderr(&verified));

    // Back into the database that holds the blob: every line imports, and `verify` finds every reference bound.
    let out = persia(&[
        "import",
        arg(&source),
        "--collection",
        "files",
        "--input",
        arg(&input),
        "--json",
    ]);
    assert_eq!(json(&out)["documents"], 2);
    let verified = persia(&["verify", "--deep", arg(&source)]);
    assert!(verified.status.success(), "{}", stderr(&verified));
}

/// A `BlobRef` with the right `blob_id` but another `size` or `chunk_size` than the blob it names, given back to the
/// database that holds the blob: the put refuses it (`check_ref`, SPEC §9.4), the import stops at its line and logs
/// nothing for it. `check_ref` reports the mismatch as corruption, the only kind it has, so today the command exits
/// with the corruption status 3, although the bad bytes are the input's: telling a caller's mismatched reference
/// from stored damage, and the exit status that follows, is ROADMAP 10.1.33. This pins today's status.
#[test]
fn a_blob_reference_of_another_size_or_chunk_size_stops_the_import_at_its_line() {
    let _serial = serial();
    for field in ["size", "chunk_size"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let dumped = with_a_blob(&path);
        let mut doc: Json = serde_json::from_str(&dumped[0]).unwrap();
        assert_eq!(doc["id"], "f1");
        let n = doc["blob"][field].as_u64().unwrap();
        doc["blob"][field] = Json::from(n + 1);
        doc["id"] = Json::from("changed");
        let input = input_file(dir.path(), format!("{}\n{doc}\n", dumped[1]).as_bytes());
        let before = dump(&path, "files");
        let out = persia(&[
            "import",
            arg(&path),
            "--collection",
            "files",
            "--input",
            arg(&input),
        ]);
        assert_eq!(out.status.code(), Some(3), "{field}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(
            err.contains("line 2: putting the document \"changed\""),
            "{field}: {err}"
        );
        assert!(
            err.contains("documents put before it, imported: 1;"),
            "{field}: {err}"
        );
        // Line 1 replaced `f2` with itself; nothing was logged for line 2.
        assert_eq!(dump(&path, "files"), before, "{field}");
        let verified = persia(&["verify", arg(&path)]);
        assert!(verified.status.success(), "{field}: {}", stderr(&verified));
    }
}

/// A database with three notes, committed, and its writer closed.
fn three_notes(path: &Path) {
    empty_notes(path);
    let dir = path.parent().unwrap();
    let input = input_file(
        dir,
        b"{\"id\": \"n1\", \"body\": \"the quick brown fox\"}\n\
          {\"id\": \"n2\", \"body\": \"a lazy dog\", \"n\": 2}\n\
          {\"id\": \"n3\", \"body\": \"quick thinking\"}\n",
    );
    stdout(&persia(&import_args(path, &input)));
}

#[test]
fn search_by_text_prints_each_hit_with_its_rank_score_and_document() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    three_notes(&path);
    let text = stdout(&persia(&[
        "search",
        arg(&path),
        "--collection",
        "notes",
        "--query",
        "quick fox",
    ]));
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("hits              1"));
    assert_eq!(lines.next(), Some("relaxation        exact"));
    assert_eq!(lines.next(), Some(""));
    let hit: Vec<&str> = lines.next().unwrap().split('\t').collect();
    assert_eq!(hit[0], "1");
    assert!(hit[1].parse::<f64>().unwrap() > 0.0, "{hit:?}");
    assert_eq!(hit[2], r#"{"id":"n1","body":"the quick brown fox"}"#);
    assert_eq!(lines.next(), None);

    // A word no document holds is relaxed away (SPEC §7.6): never zero results by default, and the output says how.
    let text = stdout(&persia(&[
        "search",
        arg(&path),
        "--collection",
        "notes",
        "--query",
        "quick zebra",
    ]));
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("hits              2"));
    let relaxation = lines.next().unwrap();
    assert!(
        relaxation.starts_with("relaxation        drop_terms ("),
        "{relaxation}"
    );
}

/// `--json` writes the response JSON of SPEC §7.5.2: exactly what the embedded API's `SearchResponse::to_json` writes
/// for the same request on the same database.
#[test]
fn search_json_is_the_embedded_apis_response_json() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    three_notes(&path);
    for request in [
        r#"{"q": "quick", "limit": 1, "relax": false}"#,
        r#"{"q": "dog", "projection": ["n"]}"#,
        r#"{"q": "zebra quick"}"#,
    ] {
        let out = persia(&[
            "search",
            arg(&path),
            "--collection",
            "notes",
            "--request",
            request,
            "--json",
        ]);
        let written = stdout(&out);
        let db = persia::OpenOptions::new()
            .read_only(true)
            .open(&path)
            .unwrap();
        let expected = db
            .collection("notes")
            .unwrap()
            .search_json(request)
            .unwrap()
            .to_json()
            .unwrap();
        db.close().unwrap();
        assert_eq!(written, format!("{expected}\n"), "{request}");
    }
    let by_text = json(&persia(&[
        "search",
        arg(&path),
        "--collection",
        "notes",
        "--query",
        "quick",
        "--json",
    ]));
    assert_eq!(by_text["hits"].as_array().unwrap().len(), 2, "{by_text}");
}

#[cfg(unix)]
#[test]
fn search_runs_beside_a_live_writer_and_sees_what_it_made_durable() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let schema = notes_schema();
    let mut writer = Database::create(&path, DatabaseOptions::default()).unwrap();
    writer.create_collection(schema.clone()).unwrap();
    let doc =
        Document::from_json(r#"{"id": "w1", "body": "written not committed"}"#, &schema).unwrap();
    writer.put("notes", &doc, Durability::Fsync).unwrap();
    let out = json(&persia(&[
        "search",
        arg(&path),
        "--collection",
        "notes",
        "--query",
        "written",
        "--json",
    ]));
    assert_eq!(out["hits"][0]["document"]["id"], "w1", "{out}");
    drop(writer);
}

#[test]
fn search_fails_for_a_bad_request_or_collection_and_needs_one_query() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    three_notes(&path);
    let out = persia(&[
        "search",
        arg(&path),
        "--collection",
        "notes",
        "--request",
        r#"{"query": "x"}"#,
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("searching the collection \"notes\""),
        "{}",
        stderr(&out)
    );
    assert_eq!(out.stdout, b"");
    let out = persia(&[
        "search",
        arg(&path),
        "--collection",
        "other",
        "--query",
        "x",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("has no collection \"other\" (it has: \"notes\")"),
        "{}",
        stderr(&out)
    );
    // Exactly one of --query and --request: a usage error otherwise.
    for args in [
        &["search", arg(&path), "--collection", "notes"][..],
        &[
            "search",
            arg(&path),
            "--collection",
            "notes",
            "--query",
            "a",
            "--request",
            "{}",
        ],
    ] {
        assert_eq!(persia(args).status.code(), Some(2), "{args:?}");
    }
}

/// The command has the built-in analyzers only: a schema naming a registered one fails both commands' opens (status
/// 1), and leaves the file as it was.
#[test]
fn import_and_search_refuse_a_schema_naming_a_registered_analyzer() {
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
    let mut db = Database::create(&path, options).unwrap();
    db.create_collection(schema).unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    let input = input_file(dir.path(), b"{\"id\": \"a\"}\n");
    for out in [
        persia(&import_args(&path, &input)),
        persia(&[
            "search",
            arg(&path),
            "--collection",
            "notes",
            "--query",
            "a",
        ]),
    ] {
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(stderr(&out).contains("tags@1"), "{}", stderr(&out));
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// A UTF-8 byte order mark at the start of the input is refused at line 1, saying so; nothing is imported. U+FEFF on a
/// later line is no mark: that line is a bad document like any other.
#[test]
fn an_input_starting_with_a_byte_order_mark_is_refused_at_line_1() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    empty_notes(&path);
    let input = input_file(
        dir.path(),
        "\u{feff}{\"id\": \"a\"}\n{\"id\": \"b\"}\n".as_bytes(),
    );
    let out = persia(&import_args(&path, &input));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("line 1: the input starts with a UTF-8 byte order mark (U+FEFF)"),
        "{}",
        stderr(&out)
    );
    assert_eq!(ids(&path), Vec::<String>::new());

    let input = input_file(
        dir.path(),
        "{\"id\": \"a\"}\n\u{feff}{\"id\": \"b\"}\n".as_bytes(),
    );
    let out = persia(&import_args(&path, &input));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("line 2: not a document"),
        "{}",
        stderr(&out)
    );
    assert_eq!(ids(&path), ["a"]);
}

/// The `i`-th line of [`a_killed_import_leaves_a_prefix_of_its_lines_and_resumes`]'s input: about 200 bytes.
fn crash_line(i: usize) -> String {
    format!(
        r#"{{"id": "k{i:05}", "n": {i}, "body": "line {i} of a long import: the quick brown fox jumps over the lazy dog, a seeded filler that makes each line about two hundred bytes long, {}"}}"#,
        i % 97
    )
}

/// A `persia import` killed (SIGKILL) during a long standard input: the database reopens with the documents of a
/// prefix of the lines, in input order (the log keeps their order, SPEC §8.2.1), and an import of the lines from the
/// next one on completes it. The kill is triggered by a file-system observation (the container grew by 2 MiB of WAL
/// chunks while the input was fed), not by time.
#[cfg(unix)]
#[test]
fn a_killed_import_leaves_a_prefix_of_its_lines_and_resumes() {
    const LINES: usize = 40_000;
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    empty_notes(&path);
    let initial = std::fs::metadata(&path).unwrap().len();
    let mut child = Command::new(env!("CARGO_BIN_EXE_persia"))
        .args(["import", arg(&path), "--collection", "notes"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut fed = 0;
    let mut triggered = false;
    for i in 0..LINES {
        let line = format!("{}\n", crash_line(i));
        // The pipe holds 64 KiB: a write blocks until the import has read the lines before it.
        if stdin.write_all(line.as_bytes()).is_err() {
            break;
        }
        fed = i + 1;
        if i % 64 == 0 && std::fs::metadata(&path).unwrap().len() > initial + (2 << 20) {
            triggered = true;
            break;
        }
    }
    assert!(
        triggered,
        "the container never grew by 2 MiB while {fed} of {LINES} lines were fed"
    );
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "killed: {status}");
    drop(stdin);

    let imported = ids(&path);
    assert!(
        !imported.is_empty(),
        "the WAL chunks the file grew by hold documents"
    );
    assert!(
        imported.len() <= fed,
        "{} documents of {fed} lines fed",
        imported.len()
    );
    let expected: Vec<String> = (0..imported.len()).map(|i| format!("k{i:05}")).collect();
    assert!(
        imported == expected,
        "the documents are a prefix of the lines, in order"
    );

    // Resume from the next line: everything is imported, once each, in order.
    let mut rest = String::new();
    for i in imported.len()..LINES {
        rest.push_str(&crash_line(i));
        rest.push('\n');
    }
    let input = input_file(dir.path(), rest.as_bytes());
    let done = json(&persia(&[
        "import",
        arg(&path),
        "--collection",
        "notes",
        "--input",
        arg(&input),
        "--json",
    ]));
    assert_eq!(
        done["documents"].as_u64().unwrap(),
        u64::try_from(LINES - imported.len()).unwrap()
    );
    let all = ids(&path);
    assert_eq!(all.len(), LINES);
    assert!(
        all.iter()
            .enumerate()
            .all(|(i, id)| *id == format!("k{i:05}")),
        "every line imported once, in order"
    );
}

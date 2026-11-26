//! Documents and blobs through the embedded API: `put`, `put_batch`, `delete`, `get`, `commit`, blobs (ROADMAP
//! 10.1.3; SPEC §6.4, §8.1, §8.2, §9.6).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::io::Read as _;

use persia::{
    Blob, BlobRef, Collection, Db, Document, Durability, ErrorCode, FieldKind, OpenOptions,
    PutBlobOptions, SchemaBuilder, TextOptions, Value,
};

fn notes(db: &Db) -> Collection {
    let schema = SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    db.create_collection(schema).unwrap()
}

fn note(c: &Collection, id: &str, body: &str) -> Document {
    let field = c.schema().unwrap().field("body").unwrap().id();
    Document::new(id).with(field, Value::String(body.to_owned()))
}

fn body(c: &Collection, id: &str) -> Option<String> {
    let field = c.schema().unwrap().field("body").unwrap().id();
    c.get(id).unwrap().map(|d| match d.get(field) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{other:?}"),
    })
}

#[test]
fn put_get_replace_delete_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    let c = notes(&db);
    c.put(&note(&c, "a", "first")).unwrap();
    assert_eq!(body(&c, "a").as_deref(), Some("first"));
    c.put(&note(&c, "a", "second")).unwrap();
    assert_eq!(body(&c, "a").as_deref(), Some("second"));
    c.put_batch(&[note(&c, "b", "bee"), note(&c, "c", "sea")])
        .unwrap();
    c.delete("c").unwrap();
    c.delete("never").unwrap();
    assert_eq!(body(&c, "c"), None);
    db.commit().unwrap();
    c.put(&note(&c, "d", "after the commit")).unwrap();
    c.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(&path).unwrap();
    let c = db.collection("notes").unwrap();
    assert_eq!(body(&c, "a").as_deref(), Some("second"));
    assert_eq!(body(&c, "b").as_deref(), Some("bee"));
    assert_eq!(body(&c, "c"), None);
    assert_eq!(body(&c, "d").as_deref(), Some("after the commit"));
}

#[test]
fn writes_that_were_not_committed_survive_a_reopen() {
    // Fsync by default: a put that returned is in the log, and the next open replays it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    let c = notes(&db);
    c.put(&note(&c, "a", "logged")).unwrap();
    c.clone()
        .with_durability(Durability::Buffered)
        .put(&note(&c, "b", "buffered"))
        .unwrap();
    db.close().unwrap();
    let db = Db::open(&path).unwrap();
    let c = db.collection("notes").unwrap();
    assert_eq!(body(&c, "a").as_deref(), Some("logged"));
    assert_eq!(
        body(&c, "b").as_deref(),
        Some("buffered"),
        "the close committed it"
    );
}

#[test]
fn json_in_and_out() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = db
        .create_collection_json(
            r#"{"name": "products", "fields": {"title": {"type": "text"}, "price": {"type": "f64", "stored": true}}}"#,
        )
        .unwrap();
    c.put_json(r#"{"id": "p1", "title": "Lamp", "price": 19.5}"#)
        .unwrap();
    assert_eq!(
        c.get_json("p1").unwrap().as_deref(),
        Some(r#"{"id":"p1","price":19.5,"title":"Lamp"}"#)
    );
    assert_eq!(c.get_json("p2").unwrap(), None);
    for bad in [
        "not json",
        r#"{"title": "no id"}"#,
        r#"{"id": "p1", "price": "free"}"#,
        r#"{"id": "p1", "unknown": 1}"#,
        r#"{"id": ""}"#,
    ] {
        let err = c.put_json(bad).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{bad}: {err}");
    }
    let long_id = format!(r#"{{"id": "{}"}}"#, "x".repeat(513));
    assert_eq!(c.put_json(&long_id).unwrap_err().code(), ErrorCode::Limit);
}

#[test]
fn a_rejected_document_ends_a_batch_and_keeps_the_ones_before() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = notes(&db);
    let bad = Document::new("");
    let err = c
        .put_batch(&[note(&c, "a", "kept"), bad, note(&c, "z", "never")])
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
    assert_eq!(body(&c, "a").as_deref(), Some("kept"));
    assert_eq!(body(&c, "z"), None);
    // The writer is not poisoned by a rejection.
    c.put(&note(&c, "b", "fine")).unwrap();
}

#[test]
fn writes_to_a_missing_collection_or_a_read_only_database_fail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    let c = notes(&db);
    c.put(&note(&c, "a", "x")).unwrap();
    db.drop_collection("notes").unwrap();
    for err in [
        c.put(&Document::new("a")).unwrap_err(),
        c.put_batch(&[Document::new("a")]).unwrap_err(),
        c.delete("a").unwrap_err(),
        c.get("a").unwrap_err(),
        c.put_json(r#"{"id": "a"}"#).unwrap_err(),
    ] {
        assert_eq!(err.code(), ErrorCode::NotFound, "{err}");
    }
    let c = notes(&db);
    c.put(&note(&c, "a", "x")).unwrap();
    db.close().unwrap();

    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    let c = reader.collection("notes").unwrap();
    assert_eq!(body(&c, "a").as_deref(), Some("x"));
    let blob = Blob::from_reader(&b"bytes"[..]);
    for err in [
        c.put(&Document::new("b")).unwrap_err(),
        c.put_batch(&[Document::new("b")]).unwrap_err(),
        c.delete("a").unwrap_err(),
        c.put_with_blob(&Document::new("b"), blob).unwrap_err(),
        c.commit().unwrap_err(),
        reader.commit().unwrap_err(),
        reader
            .put_blob(&b"x"[..], &PutBlobOptions::default())
            .unwrap_err(),
    ] {
        assert_eq!(err.code(), ErrorCode::ReadOnly, "{err}");
    }
}

#[test]
fn every_call_fails_closed_after_close() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = notes(&db);
    let blob = db
        .put_blob(&b"hello"[..], &PutBlobOptions::default())
        .unwrap();
    let mut reader = db.get_blob(&blob).unwrap();
    db.close().unwrap();
    for err in [
        c.put(&Document::new("a")).unwrap_err(),
        c.put_batch(&[Document::new("a")]).unwrap_err(),
        c.delete("a").unwrap_err(),
        c.get("a").unwrap_err(),
        c.get_json("a").unwrap_err(),
        c.put_json(r#"{"id": "a"}"#).unwrap_err(),
        c.commit().unwrap_err(),
        c.put_with_blob(&Document::new("a"), Blob::Stored(blob.clone()))
            .unwrap_err(),
    ] {
        assert_eq!(err.code(), ErrorCode::Closed, "{err}");
    }
    let mut out = Vec::new();
    let io = reader.read_to_end(&mut out).unwrap_err();
    let inner = io
        .get_ref()
        .unwrap()
        .downcast_ref::<persia::Error>()
        .unwrap();
    assert_eq!(inner.code(), ErrorCode::Closed);
}

#[test]
fn blobs_stored_read_and_referenced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    let files = db
        .create_collection_json(r#"{"name": "files", "fields": {}}"#)
        .unwrap();
    let bytes: Vec<u8> = (0..(9u32 << 20))
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();

    // Bytes with the document: stored, then the document logged with the reference.
    let attached = files
        .put_with_blob(&Document::new("big"), Blob::from_reader(&bytes[..]))
        .unwrap();
    assert_eq!(attached.size(), u64::try_from(bytes.len()).unwrap());
    let got = files.get("big").unwrap().unwrap();
    assert_eq!(got.blob_ref(), Some(&attached));
    // Streamed in windows: more than one of them here.
    let debug = format!("{:?}", db.get_blob(&attached).unwrap());
    assert!(
        debug.starts_with("BlobReader") && debug.contains("offset: 0"),
        "{debug}"
    );
    let mut streamed = Vec::new();
    db.get_blob(&attached)
        .unwrap()
        .read_to_end(&mut streamed)
        .unwrap();
    assert_eq!(streamed, bytes);
    assert_eq!(
        db.get_blob_range(&attached, 5..10).unwrap(),
        bytes[5..10].to_vec()
    );

    // From a file.
    let on_disk = dir.path().join("upload.bin");
    std::fs::write(&on_disk, b"from a file").unwrap();
    let from_path = files
        .put_with_blob(&Document::new("file"), Blob::from_path(&on_disk))
        .unwrap();
    assert_eq!(db.get_blob_range(&from_path, 0..4).unwrap(), b"from");
    let missing = files
        .put_with_blob(
            &Document::new("nofile"),
            Blob::from_path(dir.path().join("missing.bin")),
        )
        .unwrap_err();
    assert_eq!(missing.code(), ErrorCode::NotFound, "{missing}");
    assert!(files.get("nofile").unwrap().is_none());

    // Stored on its own, then referenced: by `Blob::Stored`, or carried by the document.
    let alone = db
        .put_blob(&b"alone"[..], &PutBlobOptions::default())
        .unwrap();
    files
        .put_with_blob(&Document::new("ref"), Blob::Stored(alone.clone()))
        .unwrap();
    let mut carried = Document::new("carried");
    carried.set_blob_ref(Some(alone.clone()));
    files.put(&carried).unwrap();
    files
        .put_batch(&[carried.clone(), Document::new("plain")])
        .unwrap();
    assert_eq!(
        files.get("carried").unwrap().unwrap().blob_ref(),
        Some(&alone)
    );
    db.close().unwrap();

    // Read back after a reopen, and by a read-only handle.
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    let mut back = Vec::new();
    reader
        .get_blob(&attached)
        .unwrap()
        .read_to_end(&mut back)
        .unwrap();
    assert_eq!(back, bytes);
    assert_eq!(reader.get_blob_range(&alone, 0..5).unwrap(), b"alone");
}

#[test]
fn a_reference_the_store_lacks_is_not_found_and_logs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("a.persia")).unwrap();
    let other = Db::open(dir.path().join("b.persia")).unwrap();
    let foreign: BlobRef = other
        .put_blob(&b"elsewhere"[..], &PutBlobOptions::default())
        .unwrap();
    let files = db
        .create_collection_json(r#"{"name": "files", "fields": {}}"#)
        .unwrap();

    let err = files
        .put_with_blob(&Document::new("x"), Blob::Stored(foreign.clone()))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::NotFound, "{err}");
    let mut doc = Document::new("y");
    doc.set_blob_ref(Some(foreign.clone()));
    assert_eq!(files.put(&doc).unwrap_err().code(), ErrorCode::NotFound);
    let err = files.put_batch(&[Document::new("first"), doc]).unwrap_err();
    assert_eq!(err.code(), ErrorCode::NotFound);
    for id in ["x", "y", "first"] {
        assert!(files.get(id).unwrap().is_none(), "{id} was logged");
    }
    assert_eq!(
        db.get_blob(&foreign).unwrap_err().code(),
        ErrorCode::NotFound
    );
    assert_eq!(
        db.get_blob_range(&foreign, 0..1).unwrap_err().code(),
        ErrorCode::NotFound
    );
}

/// ROADMAP 10.1.33, SPEC §9.6: a reference given to a put that names a stored blob with another size or chunk size
/// is the caller's error (`InvalidArgument`), not damage, and logs nothing, on every put path. A read given the
/// same hand-built reference is refused the same way: the reference is the caller's input (SPEC §9.6).
#[test]
fn a_reference_of_another_size_or_chunk_size_is_an_invalid_argument_and_logs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("a.persia")).unwrap();
    let files = db
        .create_collection_json(r#"{"name": "files", "fields": {}}"#)
        .unwrap();
    let ours = db
        .put_blob(&b"0123456789"[..], &PutBlobOptions::default())
        .unwrap();
    for (size, chunk) in [
        (ours.size() + 1, ours.chunk_size()),
        (ours.size(), ours.chunk_size() + 1),
    ] {
        let wrong = BlobRef::new(
            ours.blob_id(),
            size,
            ours.mime(),
            chunk,
            ours.user_meta().clone(),
        )
        .unwrap();
        let err = files
            .put_with_blob(&Document::new("x"), Blob::Stored(wrong.clone()))
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
        let mut doc = Document::new("y");
        doc.set_blob_ref(Some(wrong.clone()));
        assert_eq!(
            files.put(&doc).unwrap_err().code(),
            ErrorCode::InvalidArgument
        );
        let err = files.put_batch(&[Document::new("first"), doc]).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
        for id in ["x", "y", "first"] {
            assert!(files.get(id).unwrap().is_none(), "{id} was logged");
        }
        let err = db.get_blob(&wrong).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
        let err = db.get_blob_range(&wrong, 0..1).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
    }
    // The reference the store returned is taken.
    files
        .put_with_blob(&Document::new("ok"), Blob::Stored(ours))
        .unwrap();
    assert!(files.get("ok").unwrap().is_some());
}

#[test]
fn blob_options_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let bad_mime = PutBlobOptions::new().mime("");
    let err = db.put_blob(&b"x"[..], &bad_mime).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
    let wrong_size = PutBlobOptions::new().expected_size(2);
    let err = db.put_blob(&b"xyz"[..], &wrong_size).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
    let blob = db
        .put_blob(&b"xyz"[..], &PutBlobOptions::default())
        .unwrap();
    assert_eq!(
        db.get_blob_range(&blob, 2..4).unwrap_err().code(),
        ErrorCode::InvalidArgument
    );
}

#[test]
fn a_handle_writes_at_the_database_durability_unless_it_chose_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let db = OpenOptions::new()
        .durability(Durability::Async)
        .open(dir.path().join("app.persia"))
        .unwrap();
    let c = notes(&db);
    assert_eq!(c.durability(), Durability::Async);
    assert_eq!(
        db.collection("notes").unwrap().durability(),
        Durability::Async
    );
    let own = c.clone().with_durability(Durability::Buffered);
    assert_eq!(own.durability(), Durability::Buffered);
    assert_eq!(
        c.durability(),
        Durability::Async,
        "a clone chose, not the handle"
    );
}

#[test]
fn a_read_only_database_reads_no_blob_bytes() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting(Arc<AtomicUsize>);
    impl std::io::Read for Counting {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    notes(&db);
    db.close().unwrap();
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    let c = reader.collection("notes").unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let err = c
        .put_with_blob(
            &Document::new("a"),
            Blob::from_reader(Counting(Arc::clone(&reads))),
        )
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::ReadOnly, "{err}");
    let err = reader
        .put_blob(Counting(Arc::clone(&reads)), &PutBlobOptions::default())
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::ReadOnly, "{err}");
    assert_eq!(reads.load(Ordering::SeqCst), 0, "refused before reading");
}

#[test]
fn a_blob_reader_serves_nothing_once_closed_even_from_its_window() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let blob = db
        .put_blob(&b"0123456789"[..], &PutBlobOptions::default())
        .unwrap();
    let mut reader = db.get_blob(&blob).unwrap();
    let mut first = [0u8; 1];
    reader.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"0", "the window now holds the other nine bytes");
    db.close().unwrap();
    let io = reader.read(&mut [0u8; 4]).unwrap_err();
    let inner = io
        .get_ref()
        .unwrap()
        .downcast_ref::<persia::Error>()
        .unwrap();
    assert_eq!(inner.code(), ErrorCode::Closed);
}

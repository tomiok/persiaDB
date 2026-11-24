//! `Collection` handles: create, get, drop, schema inspection (ROADMAP 10.1.2; SPEC §6.2, §8.1).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use persia::{
    Collection, CollectionSchema, Db, ErrorCode, FieldKind, KeywordOptions, OpenOptions,
    SchemaBuilder, TextOptions,
};

fn products() -> CollectionSchema {
    SchemaBuilder::new("products")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("sku", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

#[test]
fn create_get_list_and_drop() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    assert_eq!(db.collection_names().unwrap(), Vec::<String>::new());

    let created = db.create_collection(products()).unwrap();
    assert_eq!(created.name(), "products");
    assert_eq!(created.schema().unwrap(), products());
    let err = db.create_collection(products()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::AlreadyExists, "{err}");

    db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)
        .unwrap();
    assert_eq!(db.collection_names().unwrap(), ["notes", "products"]);
    let got = db.collection("products").unwrap();
    assert_eq!(got.schema().unwrap(), products());
    let missing = db.collection("nope").unwrap_err();
    assert_eq!(missing.code(), ErrorCode::NotFound);
    assert!(missing.message().contains("nope"), "{missing}");

    db.drop_collection("products").unwrap();
    assert_eq!(db.collection_names().unwrap(), ["notes"]);
    for handle in [&created, &got] {
        assert_eq!(handle.schema().unwrap_err().code(), ErrorCode::NotFound);
    }
    assert_eq!(
        db.drop_collection("products").unwrap_err().code(),
        ErrorCode::NotFound
    );
    assert_eq!(
        db.collection("products").unwrap_err().code(),
        ErrorCode::NotFound
    );

    // A collection created again under the name is the one the old handles reach.
    let again = SchemaBuilder::new("products").build().unwrap();
    db.create_collection(again.clone()).unwrap();
    assert_eq!(created.schema().unwrap(), again);
    db.close().unwrap();
}

#[test]
fn collections_and_schemas_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    db.create_collection(products()).unwrap();
    db.create_collection_json(r#"{"name": "dropped", "fields": {}}"#)
        .unwrap();
    db.drop_collection("dropped").unwrap();
    // Not even a close: the creation and the drop were committed when they returned.
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.collection_names().unwrap(), ["products"]);
    assert_eq!(
        db.collection("products").unwrap().schema().unwrap(),
        products()
    );
}

#[test]
fn invalid_schema_json_is_an_invalid_argument() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    for bad in [
        "",
        "not json",
        r#"{"name": "x", "unknown": 1}"#,
        r#"{"name": "bad name", "fields": {}}"#,
        r#"{"name": "x", "fields": {"f": {"type": "nope"}}}"#,
    ] {
        let err = db.create_collection_json(bad).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{bad:?}: {err}");
    }
    assert_eq!(db.collection_names().unwrap(), Vec::<String>::new());
}

#[test]
fn a_read_only_database_shows_collections_and_creates_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = Db::open(&path).unwrap();
    writer.create_collection(products()).unwrap();
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    assert_eq!(reader.collection_names().unwrap(), ["products"]);
    assert_eq!(
        reader.collection("products").unwrap().schema().unwrap(),
        products()
    );
    let refused = reader
        .create_collection(SchemaBuilder::new("other").build().unwrap())
        .unwrap_err();
    assert_eq!(refused.code(), ErrorCode::ReadOnly, "{refused}");
    assert_eq!(
        reader.drop_collection("products").unwrap_err().code(),
        ErrorCode::ReadOnly
    );
    assert_eq!(
        reader
            .create_collection_json(r#"{"name": "other", "fields": {}}"#)
            .unwrap_err()
            .code(),
        ErrorCode::ReadOnly
    );
    // The reader keeps the state it opened: the writer's later collection is not in it.
    writer
        .create_collection_json(r#"{"name": "later", "fields": {}}"#)
        .unwrap();
    assert_eq!(reader.collection_names().unwrap(), ["products"]);
}

#[test]
fn handles_are_static_cross_threads_and_fail_closed_after_close() {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<Collection>();
    assert_send_sync_static::<Db>();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    let handle = db.create_collection(products()).unwrap();
    let copy = handle.clone();
    let from_thread = std::thread::spawn(move || copy.schema().map(|s| s.name().to_owned()))
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(from_thread, "products");
    assert!(format!("{handle:?}").contains("products"));

    db.close().unwrap();
    assert_eq!(handle.schema().unwrap_err().code(), ErrorCode::Closed);
    // The file can be reopened while an old handle lives; the old handle stays closed.
    let reopened = Db::open(&path).unwrap();
    assert_eq!(handle.schema().unwrap_err().code(), ErrorCode::Closed);
    assert_eq!(reopened.collection("products").unwrap().name(), "products");

    let read_only = OpenOptions::new().read_only(true).open(&path).unwrap();
    let reader_handle = read_only.collection("products").unwrap();
    drop(read_only);
    assert_eq!(
        reader_handle.schema().unwrap_err().code(),
        ErrorCode::Closed
    );
    reopened.close().unwrap();
}

#[test]
fn calls_on_a_closed_database_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let handle = db.create_collection(products()).unwrap();
    // Dropping is a close too.
    drop(db);
    assert_eq!(handle.schema().unwrap_err().code(), ErrorCode::Closed);
}

//! Typed documents via `serde` (ROADMAP 10.1.4; SPEC §6.4.1): `put_struct` is `put_json` of the value's JSON, and
//! `get_struct` / `to_struct` read a document's JSON output back.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::collections::BTreeMap;

use persia::{Collection, Db, Document, ErrorCode};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Product {
    id: String,
    title: String,
    sku: String,
    price: f64,
    stock: i64,
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

const SCHEMA: &str = r#"{"name": "products", "fields": {
    "title": {"type": "text"},
    "sku": {"type": "keyword", "stored": true},
    "price": {"type": "f64", "stored": true},
    "stock": {"type": "i64", "stored": true},
    "tags": {"type": "keyword", "stored": true},
    "note": {"type": "text"}
}}"#;

fn products(db: &Db) -> Collection {
    db.create_collection_json(SCHEMA).unwrap()
}

#[test]
fn a_struct_round_trips_and_is_its_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = products(&db);
    let lamp = Product {
        id: "p1".into(),
        title: "Desk lamp".into(),
        sku: "L-1".into(),
        price: 19.5,
        stock: 3,
        tags: vec!["home".into(), "light".into()],
        note: None,
    };
    c.put_struct(&lamp).unwrap();
    assert_eq!(c.get_struct::<Product>("p1").unwrap(), Some(lamp.clone()));
    assert_eq!(c.get_struct::<Product>("p2").unwrap(), None);
    // The same document as `put_json` of the value's JSON would have made.
    c.put_json(&serde_json::to_string(&lamp).unwrap()).unwrap();
    let doc = c.get("p1").unwrap().unwrap();
    assert_eq!(c.to_struct::<Product>(&doc).unwrap(), lamp);
    // Any `Deserialize` type reads the output: a map of JSON values here.
    let map: BTreeMap<String, serde_json::Value> = c.get_struct("p1").unwrap().unwrap();
    assert_eq!(map["sku"], "L-1");
}

#[test]
fn values_the_schema_refuses_are_invalid_arguments() {
    #[derive(Serialize)]
    struct NoId {
        title: String,
    }
    #[derive(Serialize)]
    struct WrongType {
        id: String,
        price: String,
    }
    #[derive(Serialize)]
    struct Extra {
        id: String,
        colour: String,
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = products(&db);

    let no_id = c.put_struct(&NoId { title: "x".into() }).unwrap_err();
    let wrong = c
        .put_struct(&WrongType {
            id: "a".into(),
            price: "free".into(),
        })
        .unwrap_err();
    let extra = c
        .put_struct(&Extra {
            id: "a".into(),
            colour: "red".into(),
        })
        .unwrap_err();
    let not_json = c
        .put_struct(&BTreeMap::from([(vec![1u8], "map keys must be strings")]))
        .unwrap_err();
    let not_object = c.put_struct(&[1, 2, 3]).unwrap_err();
    for err in [no_id, wrong, extra, not_json, not_object] {
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
    }
    assert!(c.get("a").unwrap().is_none());
}

#[test]
fn a_document_that_is_not_the_type_is_an_invalid_argument() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = products(&db);
    c.put_json(r#"{"id": "p1", "title": "no price"}"#).unwrap();
    let err = c.get_struct::<Product>("p1").unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
    assert!(std::error::Error::source(&err).is_some());
    let doc = c.get("p1").unwrap().unwrap();
    assert_eq!(
        c.to_struct::<Product>(&doc).unwrap_err().code(),
        ErrorCode::InvalidArgument
    );
    // A document of another collection does not fit this schema.
    let other = db
        .create_collection_json(
            r#"{"name": "other", "fields": {"x": {"type": "i64", "stored": true}}}"#,
        )
        .unwrap();
    other.put_json(r#"{"id": "o", "x": 1}"#).unwrap();
    let foreign = other.get("o").unwrap().unwrap();
    assert_eq!(
        c.to_struct::<BTreeMap<String, serde_json::Value>>(&foreign)
            .unwrap_err()
            .code(),
        ErrorCode::InvalidArgument
    );
    let _: Document = foreign;
}

fn product() -> impl Strategy<Value = Product> {
    (
        "[a-z0-9]{1,12}",
        "[ -~]{0,40}",
        "[A-Z0-9-]{1,10}",
        -1.0e9..1.0e9f64,
        any::<i64>(),
        proptest::collection::vec("[a-z]{1,8}", 0..4),
        proptest::option::of("[ -~]{0,20}"),
    )
        .prop_map(|(id, title, sku, price, stock, tags, note)| Product {
            id,
            title,
            sku,
            price,
            stock,
            tags,
            note,
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// Any product round-trips through `put_struct` and `get_struct`, and reads back the same from `to_struct`.
    #[test]
    fn any_struct_round_trips(products_in in proptest::collection::vec(product(), 1..6)) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("app.persia")).unwrap();
        let c = products(&db);
        let mut last = BTreeMap::new();
        for p in &products_in {
            c.put_struct(p).unwrap();
            last.insert(p.id.clone(), p.clone());
        }
        for (id, p) in &last {
            let got = c.get_struct::<Product>(id).unwrap();
            prop_assert_eq!(got.as_ref(), Some(p));
            let doc = c.get(id).unwrap().unwrap();
            prop_assert_eq!(&c.to_struct::<Product>(&doc).unwrap(), p);
        }
        db.close().unwrap();
    }
}

//! Documents: validation, the binary encoding and the JSON form (SPEC §6.4, ADR-0023).

// Helpers outside #[test] functions are test code too.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use persia_engine::document::{
    DecodeError, Document, DocumentCorruption, DocumentError, FieldSet, MAX_DATETIME,
    MAX_DOCUMENT_FIELDS, MAX_DOCUMENT_JSON_LEN, MAX_DOCUMENT_VALUES, MAX_ID_LEN, MAX_KEYWORD_LEN,
    MAX_STORED_DEPTH, MIN_DATETIME, Value,
};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, FieldType, KeywordOptions, NameProblem, SchemaBuilder,
    TextOptions,
};
use persia_format::{BlobId, BlobRef};
use proptest::prelude::*;
use serde_json::json;

/// One field of each type, ids `a`=0 … `g`=6.
fn all_types(strict: bool) -> CollectionSchema {
    SchemaBuilder::new("all")
        .field("a", FieldKind::Text(TextOptions::default()))
        .field("b", FieldKind::Keyword(KeywordOptions::default()))
        .field("c", FieldKind::I64 { stored: true })
        .field("d", FieldKind::F64 { stored: true })
        .field("e", FieldKind::Bool { stored: true })
        .field("f", FieldKind::Datetime { stored: true })
        .field("g", FieldKind::Stored)
        .strict(strict)
        .build()
        .unwrap()
}

fn id(schema: &CollectionSchema, name: &str) -> FieldId {
    schema.field(name).unwrap().id()
}

fn from_json(schema: &CollectionSchema, v: &serde_json::Value) -> Result<Document, DocumentError> {
    Document::from_json(&v.to_string(), schema)
}

fn blob() -> BlobRef {
    BlobRef::new(BlobId::from_bytes([7; 32]), 10, "a/b", 4, BTreeMap::new()).unwrap()
}

fn golden_doc(schema: &CollectionSchema) -> Document {
    Document::new("x")
        .with(id(schema, "a"), Value::String("hi".into()))
        .with(
            id(schema, "b"),
            Value::Strings(vec!["k".into(), "l".into()]),
        )
        .with(id(schema, "c"), Value::I64(-2))
        .with(id(schema, "d"), Value::F64(1.5))
        .with(id(schema, "e"), Value::Bool(true))
        .with(id(schema, "f"), Value::Datetime(1))
        .with(id(schema, "g"), Value::Json(json!({"z": null, "y": [1]})))
}

/// Golden vectors (SPEC §6.4.2): `DocStore` persists these bytes, so they must never change.
#[test]
fn encoding_golden() {
    let schema = all_types(true);
    let doc = golden_doc(&schema);
    #[rustfmt::skip]
    let expected: &[u8] = &[
        0x85, 0x01, 0x61, b'x', 0xf6, 0xa7,
        0x00, 0x62, b'h', b'i',
        0x01, 0x82, 0x61, b'k', 0x61, b'l',
        0x02, 0x21,
        0x03, 0xfb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0,
        0x04, 0xf5,
        0x05, 0x01,
        0x06, 0xa2, 0x61, b'y', 0x81, 0x01, 0x61, b'z', 0xf6,
        0xa0,
    ];
    assert_eq!(doc.encode(&schema, FieldSet::All).unwrap(), expected);
    assert_eq!(Document::decode(expected, &schema).unwrap(), doc);

    // A blob reference (its ADR-0011 bytes in a byte string) and an extra field of a non-strict collection.
    let lax = all_types(false);
    let mut doc = Document::new("x");
    doc.set_blob_ref(Some(blob()));
    doc.insert_extra("Zed", json!(-1.0));
    let mut expected = vec![0x85, 0x01, 0x61, b'x', 0x58, 40, 1];
    expected.extend([7; 32]);
    expected.extend([10, 4, 3, b'a', b'/', b'b', 0]);
    expected.extend([
        0xa0, 0xa1, 0x63, b'Z', b'e', b'd', 0xfb, 0xbf, 0xf0, 0, 0, 0, 0, 0, 0,
    ]);
    assert_eq!(doc.encode(&lax, FieldSet::All).unwrap(), expected);
    assert_eq!(Document::decode(&expected, &lax).unwrap(), doc);
}

#[test]
fn stored_field_set_leaves_out_unstored_fields() {
    let schema = SchemaBuilder::new("s")
        .field("hidden", FieldKind::I64 { stored: false })
        .field("shown", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let doc = Document::new("1")
        .with(id(&schema, "hidden"), Value::I64(5))
        .with(id(&schema, "shown"), Value::String("v".into()));
    let row = doc.encode(&schema, FieldSet::Stored).unwrap();
    assert_eq!(
        row,
        [0x85, 0x01, 0x61, b'1', 0xf6, 0xa1, 0x01, 0x61, b'v', 0xa0]
    );
    let mut expected = doc.clone();
    expected.remove(id(&schema, "hidden"));
    assert_eq!(Document::decode(&row, &schema).unwrap(), expected);
}

/// SPEC §6.4.1 (ROADMAP 3.2.6): `-0.0` enters a document as `0.0`; non-finite floats are rejected.
#[test]
fn f64_values_are_canonical() {
    let schema = all_types(true);
    let d = id(&schema, "d");
    let doc = Document::new("1").with(d, Value::F64(-0.0));
    assert!(matches!(doc.get(d), Some(Value::F64(x)) if x.to_bits() == 0));
    let doc = from_json(&schema, &json!({"id": "1", "d": -0.0})).unwrap();
    assert!(matches!(doc.get(d), Some(Value::F64(x)) if x.to_bits() == 0));
    assert_eq!(doc.to_json(&schema).unwrap(), r#"{"id":"1","d":0.0}"#);
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let err = Document::new("1")
            .with(d, Value::F64(x))
            .validate(&schema)
            .unwrap_err();
        assert!(
            matches!(err, DocumentError::OutOfRange { ref field, .. } if field == "d"),
            "{err:?}"
        );
    }
    // Encoded -0.0 or NaN in an f64 field is corruption: no writer produces it.
    let mut bytes = Document::new("1")
        .with(d, Value::F64(0.0))
        .encode(&schema, FieldSet::All)
        .unwrap();
    let at = bytes.iter().position(|&b| b == 0xfb).unwrap();
    for bits in [
        (-0.0f64).to_bits(),
        f64::NAN.to_bits(),
        f64::INFINITY.to_bits(),
    ] {
        bytes.splice(at + 1..at + 9, bits.to_be_bytes());
        assert_eq!(
            Document::decode(&bytes, &schema),
            Err(DecodeError::Corrupt {
                offset: at as u64,
                reason: DocumentCorruption::NonCanonicalFloat
            })
        );
    }
    // Numbers inside stored values are kept as given.
    let g = id(&schema, "g");
    let doc = Document::new("1").with(g, Value::Json(json!([-0.0])));
    let back = Document::decode(&doc.encode(&schema, FieldSet::All).unwrap(), &schema).unwrap();
    let Some(Value::Json(v)) = back.get(g) else {
        panic!()
    };
    assert_eq!(v[0].as_f64().unwrap().to_bits(), (-0.0f64).to_bits());
}

#[test]
fn json_values_by_type() {
    let schema = all_types(true);
    let doc = from_json(
        &schema,
        &json!({
            "id": "p1", "a": ["x", "y"], "b": "k", "c": -9_223_372_036_854_775_808i64, "d": 3, "e": false,
            "f": "2024-02-29T12:00:00.5+02:00", "g": {"n": [1, 2.5, "s", null, {"t": true}]}
        }),
    )
    .unwrap();
    assert_eq!(
        doc.get(id(&schema, "a")),
        Some(&Value::Strings(vec!["x".into(), "y".into()]))
    );
    assert_eq!(doc.get(id(&schema, "b")), Some(&Value::String("k".into())));
    assert_eq!(doc.get(id(&schema, "c")), Some(&Value::I64(i64::MIN)));
    assert_eq!(doc.get(id(&schema, "d")), Some(&Value::F64(3.0)));
    assert_eq!(doc.get(id(&schema, "e")), Some(&Value::Bool(false)));
    assert_eq!(
        doc.get(id(&schema, "f")),
        Some(&Value::Datetime(1_709_200_800_500_000))
    );
    assert_eq!(
        doc.to_json(&schema).unwrap(),
        r#"{"id":"p1","a":["x","y"],"b":"k","c":-9223372036854775808,"d":3.0,"e":false,"f":"2024-02-29T10:00:00.500000Z","g":{"n":[1,2.5,"s",null,{"t":true}]}}"#
    );
    // Datetimes as integers; null is an absent field; an empty list is a value.
    let doc = from_json(&schema, &json!({"id": "p2", "f": -1, "c": null, "a": []})).unwrap();
    assert_eq!(doc.get(id(&schema, "f")), Some(&Value::Datetime(-1)));
    assert_eq!(doc.get(id(&schema, "c")), None);
    assert_eq!(
        doc.to_json(&schema).unwrap(),
        r#"{"id":"p2","a":[],"f":"1969-12-31T23:59:59.999999Z"}"#
    );
}

#[test]
fn json_output_puts_id_and_blob_first() {
    let schema = SchemaBuilder::new("o")
        .field("Alpha", FieldKind::Bool { stored: true })
        .field("zeta", FieldKind::Bool { stored: true })
        .strict(false)
        .build()
        .unwrap();
    let mut meta = BTreeMap::new();
    meta.insert("k".to_owned(), "v".to_owned());
    let json = json!({
        "zeta": true, "Mid": 1, "Alpha": false, "id": "i",
        "blob": {"blob_id": "07".repeat(32), "size": 10, "mime": "a/b", "chunk_size": 4, "user_meta": meta}
    });
    let doc = from_json(&schema, &json).unwrap();
    let blob_id = "07".repeat(32);
    assert_eq!(
        doc.to_json(&schema).unwrap(),
        format!(
            r#"{{"id":"i","blob":{{"blob_id":"{blob_id}","chunk_size":4,"mime":"a/b","size":10,"user_meta":{{"k":"v"}}}},"Alpha":false,"Mid":1,"zeta":true}}"#
        )
    );
    assert_eq!(
        Document::from_json(&doc.to_json(&schema).unwrap(), &schema).unwrap(),
        doc
    );
}

#[test]
fn malformed_json_is_rejected() {
    let schema = all_types(false);
    let json_err = |text: &str| match Document::from_json(text, &schema) {
        Err(DocumentError::Json(msg)) => msg,
        other => panic!("{text}: {other:?}"),
    };
    assert!(json_err("[]").contains("expected an object"));
    assert!(json_err("{}").contains("missing `id`"));
    assert!(json_err(r#"{"id": 1}"#).contains("`id` must be a string"));
    assert!(json_err(r#"{"id": "1", "id": "2"}"#).contains("duplicate key"));
    assert!(json_err(r#"{"id": "1", "g": {"k": 1, "k": 2}}"#).contains("duplicate key"));
    assert!(json_err(r#"{"id": "1", "X": [{"k": 1, "k": 1}]}"#).contains("duplicate key"));
    assert!(json_err(r#"{"id": "1"} x"#).contains("trailing"));
    assert!(json_err(r#"{"id": "1""#).contains("EOF"));
    let deep = format!(
        r#"{{"id": "1", "g": {}{}}}"#,
        "[".repeat(200),
        "]".repeat(200)
    );
    assert!(json_err(&deep).contains("recursion limit"));
    let big = format!(
        r#"{{"id": "1", "a": "{}"}}"#,
        "x".repeat(MAX_DOCUMENT_JSON_LEN)
    );
    assert!(json_err(&big).contains("at most"));
}

/// Every rule of SPEC §6.4.1, with the error naming the field (and list index) at fault.
#[test]
#[allow(clippy::too_many_lines)] // one table of cases
fn invalid_documents_are_rejected_precisely() {
    let strict = all_types(true);
    let lax = all_types(false);
    let wrong =
        |field: &str, expected: &'static str, found: &'static str| DocumentError::WrongType {
            field: field.into(),
            expected,
            found,
        };
    let cases: Vec<(&CollectionSchema, serde_json::Value, DocumentError)> = vec![
        (&strict, json!({"id": ""}), DocumentError::IdLen { len: 0 }),
        (
            &strict,
            json!({"id": "é".repeat(257)}),
            DocumentError::IdLen { len: 514 },
        ),
        (
            &strict,
            json!({"id": "1", "zz": 1}),
            DocumentError::UnknownField { name: "zz".into() },
        ),
        (
            &strict,
            json!({"id": "1", "zz": null}),
            DocumentError::UnknownField { name: "zz".into() },
        ),
        (
            &lax,
            json!({"id": "1", "_x": 1}),
            DocumentError::FieldName {
                name: "_x".into(),
                problem: NameProblem::Reserved,
            },
        ),
        (
            &strict,
            json!({"id": "1", "ID": 1}),
            DocumentError::FieldName {
                name: "ID".into(),
                problem: NameProblem::Reserved,
            },
        ),
        (
            &lax,
            json!({"id": "1", "Blob": 1}),
            DocumentError::FieldName {
                name: "Blob".into(),
                problem: NameProblem::Reserved,
            },
        ),
        (
            &lax,
            json!({"id": "1", "a-b": 1}),
            DocumentError::FieldName {
                name: "a-b".into(),
                problem: NameProblem::InvalidChars {
                    pattern: "[A-Za-z][A-Za-z0-9_]*",
                },
            },
        ),
        (
            &strict,
            json!({"id": "1", "a": 1}),
            wrong("a", "a string or a list of strings", "an integer"),
        ),
        (
            &strict,
            json!({"id": "1", "a": ["x", null]}),
            wrong("a[1]", "a string", "null"),
        ),
        (
            &strict,
            json!({"id": "1", "b": [["x"]]}),
            wrong("b[0]", "a string", "an array"),
        ),
        (
            &strict,
            json!({"id": "1", "c": "1"}),
            wrong("c", "an i64", "a string"),
        ),
        (
            &strict,
            json!({"id": "1", "c": [1]}),
            wrong("c", "an i64", "an array"),
        ),
        (
            &strict,
            json!({"id": "1", "c": 1.5}),
            wrong("c", "an integer", "a number with a fraction or exponent"),
        ),
        (
            &strict,
            json!({"id": "1", "c": 1.0}),
            wrong("c", "an integer", "a number with a fraction or exponent"),
        ),
        (
            &strict,
            json!({"id": "1", "c": 9_223_372_036_854_775_808u64}),
            DocumentError::OutOfRange {
                field: "c".into(),
                value: "9223372036854775808".into(),
                allowed: "-2^63..2^63",
            },
        ),
        (
            &strict,
            json!({"id": "1", "d": "1"}),
            wrong("d", "an f64", "a string"),
        ),
        (
            &strict,
            json!({"id": "1", "d": [1.0, 2.0]}),
            wrong("d", "an f64", "an array"),
        ),
        (
            &strict,
            json!({"id": "1", "e": 1}),
            wrong("e", "a bool", "an integer"),
        ),
        (
            &strict,
            json!({"id": "1", "f": true}),
            wrong("f", "a datetime", "a bool"),
        ),
        (
            &strict,
            json!({"id": "1", "f": "2024-01-01"}),
            DocumentError::InvalidDatetime {
                field: "f".into(),
                value: "2024-01-01".into(),
            },
        ),
        (
            &strict,
            json!({"id": "1", "f": MAX_DATETIME + 1}),
            DocumentError::OutOfRange {
                field: "f".into(),
                value: (MAX_DATETIME + 1).to_string(),
                allowed: "0000-01-01T00:00:00Z..=9999-12-31T23:59:59.999999Z",
            },
        ),
        (
            &strict,
            json!({"id": "1", "b": ["ok", "k".repeat(MAX_KEYWORD_LEN + 1)]}),
            DocumentError::KeywordTooLong {
                field: "b[1]".into(),
                len: MAX_KEYWORD_LEN + 1,
            },
        ),
        (
            &strict,
            json!({"id": "1", "blob": 1}),
            DocumentError::Blob("expected an object, found an integer".into()),
        ),
    ];
    for (schema, json, expected) in cases {
        assert_eq!(from_json(schema, &json), Err(expected), "{json}");
    }
    // A text value has no length limit of its own; keyword values up to the limit are fine.
    let ok = json!({"id": "1".repeat(MAX_ID_LEN), "a": "t".repeat(MAX_KEYWORD_LEN + 1), "b": "k".repeat(MAX_KEYWORD_LEN)});
    assert!(from_json(&strict, &ok).is_ok());
    let deep = (0..MAX_STORED_DEPTH).fold(json!(1), |v, _| json!([v]));
    assert!(from_json(&strict, &json!({"id": "1", "g": deep})).is_ok());
    assert_eq!(
        from_json(&strict, &json!({"id": "1", "g": [deep]})),
        Err(DocumentError::TooDeep { field: "g".into() })
    );
    assert_eq!(
        from_json(&lax, &json!({"id": "1", "X": {"k": deep}})),
        Err(DocumentError::TooDeep { field: "X".into() })
    );
}

#[test]
fn invalid_blob_references_are_rejected() {
    let strict = all_types(true);
    for bad_blob in [
        json!([]),
        json!({"blob_id": "07".repeat(32), "size": 1, "mime": "a/b", "chunk_size": 4, "extra": 1}),
        json!({"blob_id": "zz", "size": 1, "mime": "a/b", "chunk_size": 4}),
        json!({"blob_id": "07".repeat(32), "size": 1, "mime": "", "chunk_size": 4}),
        json!({"blob_id": "07".repeat(32), "size": 1, "mime": "a/b", "chunk_size": 0}),
        json!({"blob_id": "07".repeat(32), "size": 1, "mime": "a/b"}),
    ] {
        let r = from_json(&strict, &json!({"id": "1", "blob": bad_blob}));
        assert!(
            matches!(r, Err(DocumentError::Blob(_))),
            "{bad_blob}: {r:?}"
        );
    }
}

/// Rules only the typed API can break.
#[test]
fn typed_documents_are_validated() {
    let schema = all_types(true);
    let lax = all_types(false);
    // A field id from another, larger schema.
    let bigger = (0..10).fold(SchemaBuilder::new("big"), |b, i| {
        b.field(format!("f{i}"), FieldKind::Stored)
    });
    let unknown = id(&bigger.build().unwrap(), "f9");
    assert_eq!(
        Document::new("1")
            .with(unknown, Value::I64(1))
            .validate(&schema),
        Err(DocumentError::UnknownFieldId { id: unknown })
    );
    assert_eq!(
        Document::new("1")
            .with(id(&schema, "g"), Value::String("s".into()))
            .validate(&schema),
        Err(DocumentError::WrongType {
            field: "g".into(),
            expected: "a JSON value",
            found: "a string"
        })
    );
    assert_eq!(
        Document::new("1")
            .with(id(&schema, "c"), Value::Datetime(1))
            .validate(&schema),
        Err(DocumentError::WrongType {
            field: "c".into(),
            expected: "an i64",
            found: "a datetime"
        })
    );
    assert_eq!(
        Document::new("1")
            .with(id(&schema, "f"), Value::Datetime(MIN_DATETIME - 1))
            .validate(&schema),
        Err(DocumentError::OutOfRange {
            field: "f".into(),
            value: (MIN_DATETIME - 1).to_string(),
            allowed: "0000-01-01T00:00:00Z..=9999-12-31T23:59:59.999999Z"
        })
    );
    assert_eq!(
        Document::new("1")
            .with(id(&schema, "g"), Value::Json(json!(null)))
            .validate(&schema),
        Err(DocumentError::NullStored { field: "g".into() })
    );
    let mut doc = Document::new("1");
    doc.insert_extra("x", json!(1));
    assert_eq!(
        doc.validate(&schema),
        Err(DocumentError::UnknownField { name: "x".into() })
    );
    assert_eq!(doc.validate(&lax), Ok(()));
    doc.insert_extra("a", json!(1));
    assert_eq!(
        doc.validate(&lax),
        Err(DocumentError::ExtraIsSchemaField { name: "a".into() })
    );
    let mut doc = Document::new("1");
    doc.insert_extra("x", json!(null));
    assert_eq!(
        doc.validate(&lax),
        Err(DocumentError::NullStored { field: "x".into() })
    );
}

#[test]
fn field_count_and_size_limits() {
    let lax = all_types(false);
    let mut doc = Document::new("1");
    for i in 0..MAX_DOCUMENT_FIELDS {
        doc.insert_extra(format!("x{i}"), json!(i));
    }
    assert_eq!(doc.validate(&lax), Ok(()));
    let mut over = doc.clone();
    over.insert(id(&lax, "e"), Value::Bool(true));
    assert_eq!(
        over.validate(&lax),
        Err(DocumentError::TooManyFields {
            count: MAX_DOCUMENT_FIELDS + 1
        })
    );

    // The binary limit applies to the whole document; 16 MiB of text plus headers is over it.
    let big = Document::new("1").with(id(&lax, "a"), Value::String("t".repeat(16 << 20)));
    assert!(matches!(big.validate(&lax), Err(DocumentError::TooLarge { len }) if len > 16 << 20));
    let fits = Document::new("1").with(id(&lax, "a"), Value::String("t".repeat((16 << 20) - 32)));
    assert_eq!(fits.validate(&lax), Ok(()));
    assert_eq!(
        Document::decode(&vec![0; (16 << 20) + 1], &lax),
        Err(DecodeError::Corrupt {
            offset: 0,
            reason: DocumentCorruption::TooLarge {
                len: (16 << 20) + 1
            }
        })
    );
}

/// An extra field kept by a non-strict collection stays an extra field after the schema adds a field of that name
/// (SPEC §6.4.2).
#[test]
fn extra_fields_survive_evolution() {
    let v1 = SchemaBuilder::new("e").strict(false).build().unwrap();
    let doc = from_json(&v1, &json!({"id": "1", "color": "red"})).unwrap();
    let row = doc.encode(&v1, FieldSet::Stored).unwrap();
    let v2 = v1
        .evolve(
            &SchemaBuilder::new("e")
                .field("color", FieldKind::Keyword(KeywordOptions::default()))
                .strict(false)
                .build()
                .unwrap(),
        )
        .unwrap();
    let old = Document::decode(&row, &v2).unwrap();
    assert_eq!(old, doc);
    assert_eq!(old.to_json(&v2).unwrap(), r#"{"id":"1","color":"red"}"#);
}

/// Each corruption of the golden encoding, with its offset.
#[test]
fn corrupt_encodings() {
    let schema = all_types(true);
    let good = golden_doc(&schema).encode(&schema, FieldSet::All).unwrap();
    let corrupt = |bytes: &[u8]| match Document::decode(bytes, &schema) {
        Err(DecodeError::Corrupt { offset, reason }) => (offset, reason),
        other => panic!("{bytes:02x?}: {other:?}"),
    };
    let patched = |at: usize, with: &[u8]| {
        let mut b = good.clone();
        b.splice(at..at + with.len(), with.iter().copied());
        b
    };
    let unexpected = |expected, found| DocumentCorruption::Unexpected { expected, found };
    assert_eq!(
        corrupt(&good[..good.len() - 1]),
        (good.len() as u64 - 1, DocumentCorruption::UnexpectedEof)
    );
    assert_eq!(
        corrupt(&[good.as_slice(), &[0]].concat()),
        (good.len() as u64, DocumentCorruption::TrailingBytes)
    );
    assert_eq!(
        corrupt(&patched(0, &[0x84])).1,
        unexpected("an array of 5 items", "an array of another length")
    );
    assert_eq!(
        corrupt(&patched(1, &[0x00])),
        (1, DocumentCorruption::OutOfRange { what: "version" })
    );
    assert_eq!(
        Document::decode(&patched(1, &[0x02]), &schema),
        Err(DecodeError::UnsupportedVersion { version: 2 })
    );
    assert_eq!(
        corrupt(&patched(2, &[0x60, 0xf6])),
        (2, DocumentCorruption::Invalid { what: "id length" })
    );
    assert_eq!(
        corrupt(&patched(4, &[0xf5])),
        (4, unexpected("a blob reference or null", "a bool"))
    );
    // Field keys: out of order, unknown.
    assert_eq!(
        corrupt(&patched(10, &[0x00])),
        (10, DocumentCorruption::KeyOrder)
    );
    assert_eq!(
        corrupt(&patched(32, &[0x07])),
        (32, DocumentCorruption::UnknownFieldId { id: 7 })
    );
    // Values of the wrong kind for the field's type.
    assert_eq!(
        corrupt(&patched(7, &[0x02])),
        (7, unexpected("a text string or an array", "an integer"))
    );
    assert_eq!(
        corrupt(&patched(17, &[0xf4])),
        (17, unexpected("an integer", "a bool"))
    );
    assert_eq!(
        corrupt(&patched(29, &[0x01])),
        (29, unexpected("a bool", "an integer"))
    );
    assert_eq!(
        corrupt(&patched(31, &[0x3b, 0x80])),
        (31, DocumentCorruption::OutOfRange { what: "datetime" })
    );
    // Integers outside i64 and the datetime range.
    let mut i64_over = good[..17].to_vec();
    i64_over.extend([0x1b, 0x80, 0, 0, 0, 0, 0, 0, 0]);
    i64_over.extend(&good[18..]);
    assert_eq!(
        corrupt(&i64_over),
        (17, DocumentCorruption::OutOfRange { what: "i64" })
    );
    let mut dt_over = good[..31].to_vec();
    dt_over.push(0x1b);
    dt_over.extend((MAX_DATETIME as u64 + 1).to_be_bytes());
    dt_over.extend(&good[32..]);
    assert_eq!(
        corrupt(&dt_over),
        (31, DocumentCorruption::OutOfRange { what: "datetime" })
    );
    // Stored map keys out of order, stored null.
    assert_eq!(
        corrupt(&patched(34, &[0x61, b'z'])),
        (38, DocumentCorruption::KeyOrder)
    );
    assert_eq!(
        corrupt(&[&good[..33], &[0xf6, 0xa0][..]].concat()),
        (
            33,
            DocumentCorruption::Invalid {
                what: "null stored value"
            }
        )
    );
    // Non-canonical heads and items outside the subset, anywhere.
    assert_eq!(corrupt(&patched(16, &[0x18, 0x02])).0, 16);
    assert_eq!(
        corrupt(&patched(4, &[0xf7])),
        (4, DocumentCorruption::UnsupportedItem { initial: 0xf7 })
    );
}

#[test]
fn corrupt_blob_ref_and_extra_fields() {
    let lax = all_types(false);
    let mut doc = Document::new("x");
    doc.set_blob_ref(Some(blob()));
    doc.insert_extra("Zed", json!(1));
    let good = doc.encode(&lax, FieldSet::All).unwrap();
    // The blob reference's own corruption, with an offset inside the document.
    let mut bad = good.clone();
    bad[6] = 0;
    match Document::decode(&bad, &lax) {
        Err(DecodeError::BlobRef(e)) => {
            assert_eq!(e.corruption().map(|(offset, _)| offset), Some(6));
        }
        other => panic!("{other:?}"),
    }
    // A byte string longer than the blob reference.
    let mut long = good[..4].to_vec();
    long.extend([0x58, 41]);
    long.extend(&good[6..46]);
    long.push(0);
    long.extend(&good[46..]);
    assert_eq!(
        Document::decode(&long, &lax),
        Err(DecodeError::Corrupt {
            offset: 46,
            reason: DocumentCorruption::TrailingBytes
        })
    );
    // Extra field names follow the field name rules.
    let mut reserved = good.clone();
    let at = reserved.len() - 4;
    reserved[at..at + 3].copy_from_slice(b"_ed");
    assert_eq!(
        Document::decode(&reserved, &lax),
        Err(DecodeError::Corrupt {
            offset: at as u64 - 1,
            reason: DocumentCorruption::Invalid {
                what: "extra field name"
            }
        })
    );
}

#[test]
fn stored_depth_limit_on_decode() {
    let schema = all_types(true);
    let g = id(&schema, "g");
    let deep = (0..MAX_STORED_DEPTH - 1).fold(json!([]), |v, _| json!([v]));
    let bytes = Document::new("1")
        .with(g, Value::Json(deep))
        .encode(&schema, FieldSet::All)
        .unwrap();
    assert!(Document::decode(&bytes, &schema).is_ok());
    // One more level: wrap the stored value in another array.
    let at = bytes.iter().position(|&b| b == 0x81).unwrap();
    let deeper = [&bytes[..at], &[0x81], &bytes[at..]].concat();
    assert_eq!(
        Document::decode(&deeper, &schema),
        Err(DecodeError::Corrupt {
            offset: at as u64 + 64,
            reason: DocumentCorruption::TooDeep
        })
    );
}

/// The version is read before the arity, so a later version that changes the array length is unsupported, not
/// corrupt (SPEC §6.4.2).
#[test]
fn version_comes_before_arity() {
    let schema = all_types(true);
    assert_eq!(
        Document::decode(&[0x86, 0x02, 0, 0, 0, 0, 0], &schema),
        Err(DecodeError::UnsupportedVersion { version: 2 })
    );
    assert_eq!(
        Document::decode(&[0x81, 0x03], &schema),
        Err(DecodeError::UnsupportedVersion { version: 3 })
    );
    let v1 = [0x86, 0x01, 0x61, b'x', 0xf6, 0xa0, 0xa0, 0xa0];
    assert_eq!(
        Document::decode(&v1, &schema),
        Err(DecodeError::Corrupt {
            offset: 0,
            reason: DocumentCorruption::Unexpected {
                expected: "an array of 5 items",
                found: "an array of another length"
            }
        })
    );
    assert_eq!(
        Document::decode(&[0x80], &schema),
        Err(DecodeError::Corrupt {
            offset: 0,
            reason: DocumentCorruption::Unexpected {
                expected: "a non-empty array",
                found: "an array"
            }
        })
    );
}

/// SPEC §6.4.1: at most `MAX_DOCUMENT_VALUES` values, so a row of one-byte items cannot make the decoder allocate
/// 30 times its size.
#[test]
fn value_count_limit() {
    let schema = all_types(true);
    let row = |elements: usize| {
        let mut b = vec![0x85, 0x01, 0x61, b'1', 0xf6, 0xa1, 0x06, 0x9a];
        b.extend(u32::try_from(elements).unwrap().to_be_bytes());
        b.extend(std::iter::repeat_n(0xf6, elements));
        b.push(0xa0);
        b
    };
    // The array is one value, its elements the others.
    assert!(Document::decode(&row(MAX_DOCUMENT_VALUES - 1), &schema).is_ok());
    assert_eq!(
        Document::decode(&row(MAX_DOCUMENT_VALUES), &schema),
        Err(DecodeError::Corrupt {
            offset: 12 + MAX_DOCUMENT_VALUES as u64 - 1,
            reason: DocumentCorruption::Invalid {
                what: "value count"
            }
        })
    );
    let nulls = |n| serde_json::Value::Array(vec![serde_json::Value::Null; n]);
    let g = id(&schema, "g");
    let fits = Document::new("1").with(g, Value::Json(nulls(MAX_DOCUMENT_VALUES - 1)));
    assert_eq!(fits.validate(&schema), Ok(()));
    let over = Document::new("1").with(g, Value::Json(nulls(MAX_DOCUMENT_VALUES)));
    assert_eq!(
        over.validate(&schema),
        Err(DocumentError::TooManyValues {
            count: MAX_DOCUMENT_VALUES + 1
        })
    );
    let list = Document::new("1").with(
        id(&schema, "b"),
        Value::Strings(vec![String::new(); MAX_DOCUMENT_VALUES]),
    );
    assert_eq!(
        list.validate(&schema),
        Err(DocumentError::TooManyValues {
            count: MAX_DOCUMENT_VALUES + 1
        })
    );
}

/// A `DocStore` row keeps `stored`-type fields and leaves out `stored: false` ones of every type (golden).
#[test]
fn stored_row_with_a_stored_field_golden() {
    let schema = SchemaBuilder::new("r")
        .field(
            "k",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                stored: false,
            }),
        )
        .field("s", FieldKind::Stored)
        .field(
            "t",
            FieldKind::Text(TextOptions {
                stored: false,
                ..TextOptions::default()
            }),
        )
        .build()
        .unwrap();
    let doc = Document::new("1")
        .with(id(&schema, "k"), Value::String("kw".into()))
        .with(id(&schema, "s"), Value::Json(json!({"a": 1})))
        .with(id(&schema, "t"), Value::String("text".into()));
    let row = doc.encode(&schema, FieldSet::Stored).unwrap();
    assert_eq!(
        row,
        [
            0x85, 0x01, 0x61, b'1', 0xf6, 0xa1, 0x01, 0xa1, 0x61, b'a', 0x01, 0xa0
        ]
    );
    let mut expected = doc;
    expected.remove(id(&schema, "k"));
    expected.remove(id(&schema, "t"));
    assert_eq!(Document::decode(&row, &schema).unwrap(), expected);
}

/// `to_json` of a typed document never recurses past the nesting limit.
#[test]
fn to_json_refuses_values_nested_too_deep() {
    let schema = all_types(false);
    let deep = (0..1000).fold(json!(1), |v, _| json!([v]));
    let doc = Document::new("1").with(id(&schema, "g"), Value::Json(deep.clone()));
    assert_eq!(
        doc.to_json(&schema),
        Err(DocumentError::TooDeep { field: "g".into() })
    );
    let mut doc = Document::new("1");
    doc.insert_extra("X", deep);
    assert_eq!(
        doc.to_json(&schema),
        Err(DocumentError::TooDeep { field: "X".into() })
    );
}

/// `to_json` of a typed document never writes JSON that reads back differently: no reserved or invalid extra
/// names (they would become the id or the blob reference), no `null` stored values (they would read as absent).
#[test]
fn to_json_refuses_what_would_not_read_back() {
    let schema = all_types(false);
    for name in ["id", "blob", "ID", "_x", "a-b"] {
        let mut doc = Document::new("1");
        doc.insert_extra(name, json!(1));
        assert!(
            matches!(doc.to_json(&schema), Err(DocumentError::FieldName { name: ref n, .. }) if n == name),
            "{name}"
        );
    }
    let mut doc = Document::new("1");
    doc.insert_extra("X", json!(null));
    assert_eq!(
        doc.to_json(&schema),
        Err(DocumentError::NullStored { field: "X".into() })
    );
    let doc = Document::new("1").with(id(&schema, "g"), Value::Json(json!(null)));
    assert_eq!(
        doc.to_json(&schema),
        Err(DocumentError::NullStored { field: "g".into() })
    );
    let doc = Document::new("1").with(id(&schema, "g"), Value::Json(json!([null])));
    assert_eq!(doc.to_json(&schema).unwrap(), r#"{"id":"1","g":[null]}"#);
}

// ---- Property tests (ROADMAP 3.2.5) ----

fn field_kind() -> impl Strategy<Value = FieldKind> {
    prop_oneof![
        any::<bool>().prop_map(|stored| FieldKind::Text(TextOptions {
            stored,
            ..TextOptions::default()
        })),
        any::<bool>().prop_map(|stored| FieldKind::Keyword(KeywordOptions {
            facet: false,
            stored
        })),
        any::<bool>().prop_map(|stored| FieldKind::I64 { stored }),
        any::<bool>().prop_map(|stored| FieldKind::F64 { stored }),
        any::<bool>().prop_map(|stored| FieldKind::Bool { stored }),
        any::<bool>().prop_map(|stored| FieldKind::Datetime { stored }),
        Just(FieldKind::Stored),
    ]
}

fn schema() -> impl Strategy<Value = CollectionSchema> {
    (
        prop::collection::btree_map("[A-Za-z][A-Za-z0-9_]{0,8}", field_kind(), 0..10),
        any::<bool>(),
    )
        .prop_filter("reserved names", |(fields, _)| {
            fields
                .keys()
                .all(|n| !n.eq_ignore_ascii_case("id") && !n.eq_ignore_ascii_case("blob"))
        })
        .prop_map(|(fields, strict)| {
            fields
                .into_iter()
                .fold(SchemaBuilder::new("p"), |b, (n, k)| b.field(n, k))
                .strict(strict)
                .build()
                .unwrap()
        })
}

fn json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        any::<u64>().prop_map(serde_json::Value::from),
        any::<f64>()
            .prop_filter("finite", |x| x.is_finite())
            .prop_map(serde_json::Value::from),
        ".{0,8}".prop_map(serde_json::Value::from),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(serde_json::Value::Array),
            prop::collection::btree_map(".{0,6}", inner, 0..6)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}

fn stored_value() -> impl Strategy<Value = serde_json::Value> {
    json_value().prop_filter("not null", |v| !v.is_null())
}

fn value(t: FieldType) -> BoxedStrategy<Value> {
    let s = || prop_oneof![".{0,12}", "[a-z ]{0,4}"];
    match t {
        FieldType::Text | FieldType::Keyword => prop_oneof![
            s().prop_map(Value::String),
            prop::collection::vec(s(), 0..4).prop_map(Value::Strings),
        ]
        .boxed(),
        FieldType::I64 => any::<i64>().prop_map(Value::I64).boxed(),
        FieldType::F64 => prop_oneof![
            any::<f64>().prop_filter("finite", |x| x.is_finite()),
            Just(-0.0),
            Just(f64::MIN_POSITIVE),
            Just(f64::MAX),
        ]
        .prop_map(Value::F64)
        .boxed(),
        FieldType::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
        FieldType::Datetime => prop_oneof![
            MIN_DATETIME..=MAX_DATETIME,
            Just(MIN_DATETIME),
            Just(MAX_DATETIME),
            -1i64..=1
        ]
        .prop_map(Value::Datetime)
        .boxed(),
        FieldType::Stored => stored_value().prop_map(Value::Json).boxed(),
    }
}

/// A schema and a valid document for it.
fn schema_and_doc() -> impl Strategy<Value = (CollectionSchema, Document)> {
    schema().prop_flat_map(|schema| {
        let fields: Vec<_> = schema
            .fields()
            .iter()
            .map(|f| (Just(f.id()), any::<bool>(), value(f.field_type())))
            .collect();
        let extra_names = if schema.strict() { 0..1 } else { 0..4 };
        let extra =
            prop::collection::btree_map("[A-Za-z][A-Za-z0-9_]{0,8}", stored_value(), extra_names);
        let blob = prop::option::of((any::<[u8; 32]>(), 0..1u64 << 40, "[ -~]{1,20}").prop_map(
            |(id, size, mime)| {
                BlobRef::new(
                    BlobId::from_bytes(id),
                    size,
                    mime,
                    BlobRef::MAX_CHUNK_SIZE,
                    BTreeMap::new(),
                )
                .unwrap()
            },
        ));
        (Just(schema), "\\PC{1,40}", fields, extra, blob).prop_map(
            |(schema, id, fields, extra, blob)| {
                let mut doc = Document::new(id);
                for (field, present, value) in fields {
                    if present {
                        doc.insert(field, value);
                    }
                }
                if !schema.strict() {
                    for (name, value) in extra {
                        let reserved =
                            name.eq_ignore_ascii_case("id") || name.eq_ignore_ascii_case("blob");
                        if !reserved && schema.field(&name).is_none() {
                            doc.insert_extra(name, value);
                        }
                    }
                }
                doc.set_blob_ref(blob);
                (schema, doc)
            },
        )
    })
}

proptest! {
    /// The binary encoding round-trips exactly, and decoding then encoding gives the same bytes.
    #[test]
    fn binary_round_trip((schema, doc) in schema_and_doc()) {
        prop_assert_eq!(doc.validate(&schema), Ok(()));
        let bytes = doc.encode(&schema, FieldSet::All).unwrap();
        let back = Document::decode(&bytes, &schema).unwrap();
        prop_assert_eq!(&back, &doc);
        prop_assert_eq!(back.encode(&schema, FieldSet::All).unwrap(), bytes);
    }

    /// A `DocStore` row decodes to the document without its unstored fields.
    #[test]
    fn stored_round_trip((schema, doc) in schema_and_doc()) {
        let row = doc.encode(&schema, FieldSet::Stored).unwrap();
        let mut expected = doc.clone();
        for field in schema.fields().iter().filter(|f| !f.kind().stored()) {
            expected.remove(field.id());
        }
        prop_assert_eq!(Document::decode(&row, &schema).unwrap(), expected);
    }

    /// JSON round-trips exactly, f64 bits and datetimes included.
    #[test]
    fn json_round_trip((schema, doc) in schema_and_doc()) {
        let json = doc.to_json(&schema).unwrap();
        let back = Document::from_json(&json, &schema).unwrap();
        prop_assert_eq!(&back, &doc);
        for (id, v) in doc.fields() {
            if let (Value::F64(a), Some(Value::F64(b))) = (v, back.get(id)) {
                prop_assert_eq!(a.to_bits(), b.to_bits());
            }
        }
        prop_assert_eq!(back.to_json(&schema).unwrap(), json);
    }

    /// A value of another type is rejected, naming the field.
    #[test]
    fn wrong_types_are_rejected((schema, doc) in schema_and_doc(), pick in any::<prop::sample::Index>(), other in any::<prop::sample::Index>()) {
        prop_assume!(!schema.fields().is_empty());
        let field = &schema.fields()[pick.index(schema.fields().len())];
        let wrong: Vec<Value> = [
            Value::String("s".into()), Value::I64(1), Value::F64(1.0), Value::Bool(true),
            Value::Datetime(0), Value::Json(json!({})),
        ]
        .into_iter()
        .filter(|v| {
            !matches!(
                (field.field_type(), v),
                (FieldType::Text | FieldType::Keyword, Value::String(_))
                    | (FieldType::I64, Value::I64(_))
                    | (FieldType::F64, Value::F64(_))
                    | (FieldType::Bool, Value::Bool(_))
                    | (FieldType::Datetime, Value::Datetime(_))
                    | (FieldType::Stored, Value::Json(_))
            )
        })
        .collect();
        let value = wrong[other.index(wrong.len())].clone();
        let found = value.kind();
        let doc = doc.with(field.id(), value);
        prop_assert!(
            matches!(doc.validate(&schema), Err(DocumentError::WrongType { field: ref f, found: fd, .. }) if f == field.name() && fd == found),
            "{:?}", doc.validate(&schema)
        );
    }

    /// Decoding damaged bytes never panics; whatever it accepts is the one canonical encoding of what it returns.
    #[test]
    fn damaged_encodings_never_panic((schema, doc) in schema_and_doc(), edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4), cut in any::<prop::sample::Index>()) {
        let mut bytes = doc.encode(&schema, FieldSet::All).unwrap();
        for (at, byte) in edits {
            let i = at.index(bytes.len());
            bytes[i] = byte;
        }
        let bytes = &bytes[..cut.index(bytes.len() + 1)];
        if let Ok(back) = Document::decode(bytes, &schema) {
            prop_assert!(back.to_json(&schema).is_ok());
            match back.encode(&schema, FieldSet::All) {
                Ok(again) => prop_assert_eq!(again, bytes),
                // What a row may hold and a new document may not (SPEC §6.4.2).
                Err(e) => prop_assert!(
                    matches!(e, DocumentError::ExtraIsSchemaField { .. } | DocumentError::UnknownField { .. }),
                    "{e:?}"
                ),
            }
        }
    }

    /// Arbitrary bytes never panic.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let _ = Document::decode(&bytes, &all_types(false));
    }
}

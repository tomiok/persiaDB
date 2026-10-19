//! The JSON form of documents and the `datetime` text form, hardened (SPEC §6.4.1, ADR-0023).
//!
//! Invariants:
//! 1. Duplicate keys are rejected at every level, whatever the key's escaping or the value.
//! 2. Reserved keys (`id`, `blob` in any letter case, a leading `_`) are never fields, strict or not.
//! 3. `null` means absent for fields, extra fields and `blob`, and is kept inside `stored` values.
//! 4. Errors name the field, and the list index where an element is at fault.
//! 5. Numbers: `f64` fields take any number (integers to the nearest `f64`, `-0` and `-0.0` to `0.0`); `stored`
//!    numbers keep their exact bits; `i64` takes exactly the integers in range.
//! 6. `datetime` strings parse like a naive proleptic Gregorian calendar over the whole range, with any offset,
//!    and are written in the one UTC form.
//! 7. Limits (id bytes, field count, JSON text length) hold at the exact boundary.

// Helpers outside #[test] functions are test code too; long table-driven tests and small casts on test data.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

use persia_engine::document::{
    Document, DocumentError, FieldSet, MAX_DATETIME, MAX_DOCUMENT_FIELDS, MAX_DOCUMENT_JSON_LEN,
    MAX_ID_LEN, MAX_KEYWORD_LEN, MIN_DATETIME, Value,
};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, NameProblem, SchemaBuilder, TextOptions,
};
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

fn parse(schema: &CollectionSchema, text: &str) -> Result<Document, DocumentError> {
    Document::from_json(text, schema)
}

fn json_error(schema: &CollectionSchema, text: &str) -> String {
    match parse(schema, text) {
        Err(DocumentError::Json(msg)) => msg,
        other => panic!("{text}: {other:?}"),
    }
}

const BLOB: &str = r#""blob_id": "0707070707070707070707070707070707070707070707070707070707070707", "mime": "a/b", "chunk_size": 4"#;

#[test]
fn duplicate_keys_are_rejected_at_every_level() {
    for strict in [true, false] {
        let schema = all_types(strict);
        for text in [
            r#"{"id": "1", "id": "1"}"#,
            r#"{"id": "1", "id": "2"}"#,
            r#"{"id": "1", "a": "x", "a": "y"}"#,
            r#"{"id": "1", "a": null, "a": "y"}"#,
            r#"{"id": "1", "c": 1, "c": 1}"#,
            r#"{"id": "1", "blob": null, "blob": null}"#,
            r#"{"id": "1", "X": 1, "X": 2}"#,
            r#"{"id": "1", "g": {"k": {"m": 1, "m": 2}}}"#,
            r#"{"id": "1", "g": [[{"k": 1, "k": 1}]]}"#,
            r#"{"id": "1", "g": {"é": 1, "é": 2}}"#,
            r#"{"id": "1", "g": {"": 1, "": 2}}"#,
        ] {
            assert!(
                json_error(&schema, text).contains("duplicate key"),
                "{text}"
            );
        }
        let in_blob = [
            format!(r#"{{"id": "1", "blob": {{{BLOB}, "size": 1, "size": 1}}}}"#),
            format!(
                r#"{{"id": "1", "blob": {{{BLOB}, "size": 1, "user_meta": {{"k": "v", "k": "w"}}}}}}"#
            ),
        ];
        for text in in_blob {
            assert!(
                json_error(&schema, &text).contains("duplicate key"),
                "{text}"
            );
        }
    }
    // Keys differ by case or by Unicode normalization: not duplicates.
    let lax = all_types(false);
    let doc = parse(
        &lax,
        r#"{"id": "1", "X": 1, "x": 2, "g": {"k": 1, "K": 2, "\u00e9": 3, "e\u0301": 4}}"#,
    )
    .unwrap();
    assert_eq!(doc.extra().len(), 2);
    let Some(Value::Json(g)) = doc.get(id(&lax, "g")) else {
        panic!()
    };
    assert_eq!(g.as_object().unwrap().len(), 4);
}

/// SPEC §6.4.1 "Keys" and §6.2.1: reserved names are errors as keys, strict or not, even with a `null` value.
#[test]
fn reserved_and_invalid_keys() {
    for strict in [true, false] {
        let schema = all_types(strict);
        for name in [
            "ID", "Id", "iD", "BLOB", "Blob", "bLoB", "blOB", "_", "_x", "__id", "_id",
        ] {
            for value in ["1", "null"] {
                let text = format!(r#"{{"id": "1", "{name}": {value}}}"#);
                assert_eq!(
                    parse(&schema, &text),
                    Err(DocumentError::FieldName {
                        name: name.into(),
                        problem: NameProblem::Reserved
                    }),
                    "{text}"
                );
            }
        }
    }
    let lax = all_types(false);
    let invalid_chars = NameProblem::InvalidChars {
        pattern: "[A-Za-z][A-Za-z0-9_]*",
    };
    for (name, problem) in [
        ("", NameProblem::Empty),
        ("1x", invalid_chars),
        ("é", invalid_chars),
        ("x y", invalid_chars),
        ("x.y", invalid_chars),
        (&*"x".repeat(65), NameProblem::TooLong { len: 65 }),
    ] {
        let text = format!(r#"{{"id": "1", "{name}": 1}}"#);
        assert_eq!(
            parse(&lax, &text),
            Err(DocumentError::FieldName {
                name: name.into(),
                problem
            }),
            "{text}"
        );
    }
    // The longest valid name is fine as an extra field; schema field names are case-sensitive.
    let long = "x".repeat(64);
    let doc = parse(&lax, &format!(r#"{{"id": "1", "{long}": 1, "A": true}}"#)).unwrap();
    assert_eq!(doc.extra().get(&long), Some(&json!(1)));
    assert_eq!(doc.extra().get("A"), Some(&json!(true)));
    assert_eq!(doc.get(id(&lax, "a")), None);
    assert_eq!(
        parse(&all_types(true), r#"{"id": "1", "A": true}"#),
        Err(DocumentError::UnknownField { name: "A".into() })
    );
}

/// SPEC §6.4.1: `null` is an absent field (and `blob`); inside a `stored` value it is kept.
#[test]
fn null_semantics() {
    for strict in [true, false] {
        let schema = all_types(strict);
        let doc = parse(
            &schema,
            r#"{"id": "1", "blob": null, "a": null, "b": null, "c": null, "d": null, "e": null, "f": null, "g": null}"#,
        )
        .unwrap();
        assert_eq!(doc, Document::new("1"));
        assert_eq!(doc.to_json(&schema).unwrap(), r#"{"id":"1"}"#);
        let doc = parse(&schema, r#"{"id": "1", "g": [null, {"k": null}]}"#).unwrap();
        assert_eq!(
            doc.to_json(&schema).unwrap(),
            r#"{"id":"1","g":[null,{"k":null}]}"#
        );
        assert!(
            json_error(&schema, r#"{"id": null}"#).contains("`id` must be a string"),
            "{strict}"
        );
        assert_eq!(
            parse(&schema, r#"{"id": "1", "b": ["x", null]}"#),
            Err(DocumentError::WrongType {
                field: "b[1]".into(),
                expected: "a string",
                found: "null"
            })
        );
    }
    // A non-strict collection drops a null extra field and keeps one holding null.
    let lax = all_types(false);
    let doc = parse(&lax, r#"{"id": "1", "X": null, "Y": [null]}"#).unwrap();
    assert_eq!(doc.extra().len(), 1);
    assert_eq!(doc.extra().get("Y"), Some(&json!([null])));
}

/// Errors name the field, and the index of the list element at fault.
#[test]
fn errors_name_field_and_list_index() {
    let schema = all_types(true);
    let cases: [(&str, DocumentError); 6] = [
        (
            r#"{"id": "1", "a": ["x", "y", 3]}"#,
            DocumentError::WrongType {
                field: "a[2]".into(),
                expected: "a string",
                found: "an integer",
            },
        ),
        (
            r#"{"id": "1", "b": ["x", {}]}"#,
            DocumentError::WrongType {
                field: "b[1]".into(),
                expected: "a string",
                found: "an object",
            },
        ),
        (
            r#"{"id": "1", "a": [true]}"#,
            DocumentError::WrongType {
                field: "a[0]".into(),
                expected: "a string",
                found: "a bool",
            },
        ),
        (
            r#"{"id": "1", "a": {}}"#,
            DocumentError::WrongType {
                field: "a".into(),
                expected: "a string or a list of strings",
                found: "an object",
            },
        ),
        (
            r#"{"id": "1", "e": [true]}"#,
            DocumentError::WrongType {
                field: "e".into(),
                expected: "a bool",
                found: "an array",
            },
        ),
        (
            r#"{"id": "1", "f": ["2024-01-01T00:00:00Z"]}"#,
            DocumentError::WrongType {
                field: "f".into(),
                expected: "a datetime",
                found: "an array",
            },
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(parse(&schema, text), Err(expected), "{text}");
    }
    let long = "k".repeat(MAX_KEYWORD_LEN + 1);
    assert_eq!(
        parse(
            &schema,
            &format!(r#"{{"id": "1", "b": ["ok", "ok", "{long}"]}}"#)
        ),
        Err(DocumentError::KeywordTooLong {
            field: "b[2]".into(),
            len: MAX_KEYWORD_LEN + 1
        })
    );
    assert_eq!(
        parse(&schema, &format!(r#"{{"id": "1", "b": "{long}"}}"#)),
        Err(DocumentError::KeywordTooLong {
            field: "b".into(),
            len: MAX_KEYWORD_LEN + 1
        })
    );
    // The typed API names the index too.
    let b = id(&schema, "b");
    let doc = Document::new("1").with(
        b,
        Value::Strings(vec![String::new(), "x".into(), "y".into(), long.clone()]),
    );
    assert_eq!(
        doc.validate(&schema),
        Err(DocumentError::KeywordTooLong {
            field: "b[3]".into(),
            len: MAX_KEYWORD_LEN + 1
        })
    );
}

/// SPEC §6.4.1 "Output": `id`, then `blob`, then fields and extra fields together in byte order of their names.
#[test]
fn json_output_order_is_by_bytes() {
    let schema = SchemaBuilder::new("o")
        .field("a_b", FieldKind::Bool { stored: true })
        .field("B", FieldKind::I64 { stored: true })
        .field("A", FieldKind::Text(TextOptions::default()))
        .strict(false)
        .build()
        .unwrap();
    let doc = parse(
        &schema,
        &format!(
            r#"{{"aa": 1, "a_b": true, "Zz": 2, "aZ": 3, "B": 4, "A": "t", "blob": {{{BLOB}, "size": 5}}, "id": "i"}}"#
        ),
    )
    .unwrap();
    let blob_id = "07".repeat(32);
    assert_eq!(
        doc.to_json(&schema).unwrap(),
        format!(
            r#"{{"id":"i","blob":{{"blob_id":"{blob_id}","chunk_size":4,"mime":"a/b","size":5}},"A":"t","B":4,"Zz":2,"aZ":3,"a_b":true,"aa":1}}"#
        )
    );
    // Strings are escaped as JSON; the id and values keep their characters.
    let doc = parse(
        &schema,
        r#"{"id": "q\"\\\n\u0000é😀", "A": ["\t", "\u001f"]}"#,
    )
    .unwrap();
    assert_eq!(doc.id(), "q\"\\\n\u{0}é😀");
    assert_eq!(
        doc.to_json(&schema).unwrap(),
        r#"{"id":"q\"\\\n\u0000é😀","A":["\t","\u001f"]}"#
    );
    assert_eq!(parse(&schema, &doc.to_json(&schema).unwrap()).unwrap(), doc);
}

/// SPEC §6.4.1: an `f64` field takes any number; integers become the nearest `f64`; `-0` and `-0.0` become
/// `0.0`. Numbers too large for an `f64` are malformed JSON.
#[test]
fn f64_fields_from_json() {
    let schema = all_types(true);
    let d = id(&schema, "d");
    let bits = |text: &str| match parse(&schema, &format!(r#"{{"id": "1", "d": {text}}}"#)) {
        Ok(doc) => match doc.get(d) {
            Some(Value::F64(x)) => x.to_bits(),
            other => panic!("{text}: {other:?}"),
        },
        Err(e) => panic!("{text}: {e:?}"),
    };
    for (text, expected) in [
        ("-0", 0.0),
        ("-0.0", 0.0),
        ("-0e5", 0.0),
        ("0", 0.0),
        ("9007199254740993", 9_007_199_254_740_992.0),
        ("18446744073709551615", 18_446_744_073_709_551_615.0),
        ("-9223372036854775808", -9_223_372_036_854_775_808.0),
        ("100000000000000000000000", 1e23),
        ("0.1", 0.1),
        ("5e-324", 5e-324),
        ("2.2250738585072014e-308", f64::MIN_POSITIVE),
        ("1.7976931348623157e308", f64::MAX),
        ("1e-400", 0.0),
    ] {
        assert_eq!(bits(text), expected.to_bits(), "{text}");
    }
    let to_json = |x: f64| {
        Document::new("1")
            .with(d, Value::F64(x))
            .to_json(&schema)
            .unwrap()
    };
    assert_eq!(
        to_json(9_007_199_254_740_992.0),
        r#"{"id":"1","d":9007199254740992.0}"#
    );
    assert_eq!(to_json(1e23), r#"{"id":"1","d":1e+23}"#);
    assert_eq!(to_json(5e-324), r#"{"id":"1","d":5e-324}"#);
    for text in ["1e400", "-1e309"] {
        let msg = json_error(&schema, &format!(r#"{{"id": "1", "d": {text}}}"#));
        assert!(msg.contains("out of range"), "{text}: {msg}");
    }
}

/// SPEC §6.4.1: an `i64` field takes "an integer in the `i64` range (no fraction or exponent)". The JSON
/// grammar's `-0` is such an integer; integers beyond the range are out of range, not fractions.
#[test]
fn i64_fields_take_exactly_the_integers_in_range() {
    let schema = all_types(true);
    let c = id(&schema, "c");
    for (text, expected) in [
        ("0", 0),
        ("-9223372036854775808", i64::MIN),
        ("9223372036854775807", i64::MAX),
        // serde_json reads `-0` as a float, so `-0.0` cannot be told apart and is 0 too.
        ("-0.0", 0),
    ] {
        let doc = parse(&schema, &format!(r#"{{"id": "1", "c": {text}}}"#)).unwrap();
        assert_eq!(doc.get(c), Some(&Value::I64(expected)), "{text}");
    }
    for text in ["1e0", "1E2", "0.0", "1.5"] {
        assert_eq!(
            parse(&schema, &format!(r#"{{"id": "1", "c": {text}}}"#)),
            Err(DocumentError::WrongType {
                field: "c".into(),
                expected: "an integer",
                found: "a number with a fraction or exponent"
            }),
            "{text}"
        );
    }
}

/// SPEC §6.4.1: JSON `-0` has no fraction or exponent, so an `i64` (or `datetime`) field takes it as 0.
#[test]
fn i64_fields_take_json_negative_zero() {
    let schema = all_types(true);
    let c = id(&schema, "c");
    // serde_json reads `-0` as the float -0.0; the parser still takes it as the integer 0.
    let doc = parse(&schema, r#"{"id": "1", "c": -0, "f": -0}"#);
    assert_eq!(
        doc.as_ref().ok().and_then(|d| d.get(c)),
        Some(&Value::I64(0)),
        "-0 in an i64 field: {doc:?}"
    );
}

/// SPEC §6.4.1 and [`DocumentError::OutOfRange`]: "an integer beyond `i64`" is out of range, whatever its size.
#[test]
fn i64_fields_reject_huge_integers_as_out_of_range() {
    let schema = all_types(true);
    // serde_json reads integers beyond u64 and below i64 as floats; they are still out of range, not fractions.
    for text in [
        "9223372036854775808",
        "-9223372036854775809",
        "18446744073709551616",
        "-100000000000000000000",
    ] {
        let r = parse(&schema, &format!(r#"{{"id": "1", "c": {text}}}"#));
        assert!(
            matches!(&r, Err(DocumentError::OutOfRange { field, .. }) if field == "c"),
            "{text}: {r:?}"
        );
    }
}

/// SPEC §6.4.1: numbers inside `stored` values (and extra fields) are kept exactly as given, `-0.0` included;
/// integers stay integers.
#[test]
fn stored_numbers_keep_their_form() {
    let lax = all_types(false);
    let g = id(&lax, "g");
    let doc = parse(
        &lax,
        r#"{"id": "1", "g": [-0.0, 0, 1.0, 18446744073709551615, -9223372036854775808, 5e-324], "X": -0.0}"#,
    )
    .unwrap();
    let Some(Value::Json(v)) = doc.get(g) else {
        panic!()
    };
    assert_eq!(v[0].as_f64().unwrap().to_bits(), (-0.0f64).to_bits());
    assert_eq!(
        doc.extra()["X"].as_f64().unwrap().to_bits(),
        (-0.0f64).to_bits()
    );
    let json = doc.to_json(&lax).unwrap();
    assert_eq!(
        json,
        r#"{"id":"1","X":-0.0,"g":[-0.0,0,1.0,18446744073709551615,-9223372036854775808,5e-324]}"#
    );
    let bytes = doc.encode(&lax, FieldSet::All).unwrap();
    let back = Document::decode(&bytes, &lax).unwrap();
    assert_eq!(back.to_json(&lax).unwrap(), json);
}

/// Through the JSON form: offsets crossing day, month and year boundaries, leap years, the range ends.
#[test]
fn datetime_strings_at_the_edges() {
    let schema = all_types(true);
    let f = id(&schema, "f");
    let utc = |text: &str| -> Option<String> {
        let doc = parse(&schema, &format!(r#"{{"id": "1", "f": "{text}"}}"#)).ok()?;
        let out: serde_json::Value = serde_json::from_str(&doc.to_json(&schema).unwrap()).unwrap();
        Some(out["f"].as_str().unwrap().to_owned())
    };
    for (text, expected) in [
        ("2000-01-01T00:30:00+01:00", "1999-12-31T23:30:00Z"),
        ("1999-12-31T23:30:00-01:00", "2000-01-01T00:30:00Z"),
        ("2024-02-28T23:00:00-01:00", "2024-02-29T00:00:00Z"),
        ("2023-02-28T23:00:00-01:00", "2023-03-01T00:00:00Z"),
        ("2024-03-01T00:00:00+00:01", "2024-02-29T23:59:00Z"),
        (
            "2024-12-31T23:59:59.999999-23:59",
            "2025-01-01T23:58:59.999999Z",
        ),
        ("0000-02-29T00:00:00Z", "0000-02-29T00:00:00Z"),
        ("2400-02-29T12:00:00z", "2400-02-29T12:00:00Z"),
        ("0000-01-01T23:59:59+23:59", "0000-01-01T00:00:59Z"),
        ("0000-01-01T00:00:00-00:00", "0000-01-01T00:00:00Z"),
        (
            "9999-12-31T23:59:59.999999+00:00",
            "9999-12-31T23:59:59.999999Z",
        ),
        ("9999-12-31T00:00:00-23:59", "9999-12-31T23:59:00Z"),
        ("1970-01-01t00:00:00.5Z", "1970-01-01T00:00:00.500000Z"),
        ("1970-01-01T00:00:00.05Z", "1970-01-01T00:00:00.050000Z"),
        ("1970-01-01T00:00:00.000000Z", "1970-01-01T00:00:00Z"),
        ("1969-12-31T23:59:59.999999Z", "1969-12-31T23:59:59.999999Z"),
    ] {
        assert_eq!(utc(text).as_deref(), Some(expected), "{text}");
    }
    for text in [
        "2100-02-29T00:00:00Z",
        "1900-02-29T00:00:00Z",
        "2023-02-29T00:00:00Z",
        "2024-06-31T00:00:00Z",
        "1970-01-01T00:00:00+00:00:00",
        "1970-01-01T00:00:00+1:00",
        "1970-01-01T00:00:00+00:60",
        "1970-01-01T00:00:00-24:00",
        "1970-01-01T00:00:00.0000000Z",
        "10000-01-01T00:00:00Z",
        "-0001-01-01T00:00:00Z",
    ] {
        let r = parse(&schema, &format!(r#"{{"id": "1", "f": "{text}"}}"#));
        assert!(
            matches!(&r, Err(DocumentError::InvalidDatetime { field, value }) if field == "f" && value == text),
            "{text}: {r:?}"
        );
    }
    // Valid text, but outside the range once the offset is applied: out of range, as for the integer form.
    for text in [
        "9999-12-31T00:01:00-23:59",
        "0000-01-01T23:58:59+23:59",
        "0000-01-01T00:00:00+00:01",
    ] {
        let r = parse(&schema, &format!(r#"{{"id": "1", "f": "{text}"}}"#));
        assert!(
            matches!(&r, Err(DocumentError::OutOfRange { field, .. }) if field == "f"),
            "{text}: {r:?}"
        );
    }
    // The integer form at the range ends.
    for v in [MIN_DATETIME, MAX_DATETIME, 0, -1] {
        let doc = parse(&schema, &format!(r#"{{"id": "1", "f": {v}}}"#)).unwrap();
        assert_eq!(doc.get(f), Some(&Value::Datetime(v)));
    }
    for v in [MIN_DATETIME - 1, MAX_DATETIME + 1, i64::MIN, i64::MAX] {
        let r = parse(&schema, &format!(r#"{{"id": "1", "f": {v}}}"#));
        assert!(
            matches!(&r, Err(DocumentError::OutOfRange { field, .. }) if field == "f"),
            "{v}: {r:?}"
        );
    }
}

/// SPEC §6.4.1 limits at their exact boundaries, through JSON.
#[test]
fn json_limits_at_the_boundary() {
    let lax = all_types(false);
    // id: bytes, not chars.
    let max_id = "é".repeat(MAX_ID_LEN / 2);
    assert!(parse(&lax, &format!(r#"{{"id": "{max_id}"}}"#)).is_ok());
    assert_eq!(
        parse(&lax, &format!(r#"{{"id": "{max_id}x"}}"#)),
        Err(DocumentError::IdLen {
            len: MAX_ID_LEN + 1
        })
    );
    // 1024 fields: schema fields and extra fields together; null keys do not count.
    let fields = r#""a": "s", "b": "k", "c": 1, "d": 1.5, "e": true, "f": 0, "g": {}"#;
    let extras = |n: usize| {
        (0..n)
            .map(|i| format!(r#""x{i:04}": {i}"#))
            .collect::<Vec<_>>()
            .join(",")
    };
    let n = MAX_DOCUMENT_FIELDS - 7;
    let full = format!(r#"{{"id": "1", {fields}, {}, "y": null}}"#, extras(n));
    let doc = parse(&lax, &full).unwrap();
    assert_eq!(
        doc.fields().count() + doc.extra().len(),
        MAX_DOCUMENT_FIELDS
    );
    assert_eq!(
        parse(
            &lax,
            &format!(r#"{{"id": "1", {fields}, {}}}"#, extras(n + 1))
        ),
        Err(DocumentError::TooManyFields {
            count: MAX_DOCUMENT_FIELDS + 1
        })
    );
    // The JSON text: exactly 16 MiB is read, one byte more is not.
    let schema = all_types(true);
    let text = |len: usize| format!(r#"{{"id":"1","a":"{}"}}"#, "t".repeat(len - 17));
    let exact = text(MAX_DOCUMENT_JSON_LEN);
    assert_eq!(exact.len(), MAX_DOCUMENT_JSON_LEN);
    assert!(parse(&schema, &exact).is_ok());
    let over = text(MAX_DOCUMENT_JSON_LEN + 1);
    assert!(json_error(&schema, &over).contains("at most"));
}

// ---- Property tests ----

const fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn month_len(y: i64, m: i64) -> i64 {
    [
        31,
        if is_leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ][m as usize - 1]
}

/// Days from 0000-01-01 to the date, by counting: an oracle independent of the engine's closed-form arithmetic.
fn days_since_year_zero(y: i64, m: i64, d: i64) -> i64 {
    let years: i64 = (0..y).map(|y| if is_leap(y) { 366 } else { 365 }).sum();
    let months: i64 = (1..m).map(|m| month_len(y, m)).sum();
    years + months + d - 1
}

fn naive_micros(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, micros: i64, offset: i64) -> i64 {
    let days = days_since_year_zero(y, mo, d) - days_since_year_zero(1970, 1, 1);
    (days * 86_400 + h * 3600 + mi * 60 + s - offset * 60) * 1_000_000 + micros
}

/// `(digits, value)` of a fraction with 1..=6 digits.
fn fraction() -> impl Strategy<Value = Option<(usize, i64)>> {
    prop::option::of((1usize..=6).prop_flat_map(|n| (Just(n), 0..10i64.pow(n as u32))))
}

/// An offset in minutes, as `Z`/`z` (None) or `±hh:mm`.
fn offset() -> impl Strategy<Value = Option<i64>> {
    prop::option::of(prop_oneof![
        -1439i64..=1439,
        Just(1439),
        Just(-1439),
        Just(0)
    ])
}

proptest! {
    /// Invariant 6: any well-formed RFC 3339 string parses to what a naive calendar says, or is rejected when
    /// the date does not exist or the instant is outside the range.
    #[test]
    fn datetime_strings_match_a_naive_calendar(
        y in prop_oneof![0i64..=9999, Just(0), Just(9999), Just(2000), Just(1900)],
        mo in 1i64..=12,
        d in 1i64..=31,
        h in 0i64..24,
        mi in 0i64..60,
        s in 0i64..60,
        frac in fraction(),
        off in offset(),
        lower in any::<(bool, bool)>(),
    ) {
        let schema = all_types(true);
        let frac_text = frac.map_or(String::new(), |(n, v)| format!(".{v:0n$}"));
        let micros = frac.map_or(0, |(n, v)| v * 10i64.pow(6 - n as u32));
        let off_text = match off {
            None => if lower.1 { "z".to_owned() } else { "Z".to_owned() },
            Some(m) => format!("{}{:02}:{:02}", if m < 0 { '-' } else { '+' }, m.abs() / 60, m.abs() % 60),
        };
        let t = if lower.0 { 't' } else { 'T' };
        let text = format!("{y:04}-{mo:02}-{d:02}{t}{h:02}:{mi:02}:{s:02}{frac_text}{off_text}");
        let r = parse(&schema, &format!(r#"{{"id": "1", "f": "{text}"}}"#));
        let expected = (d <= month_len(y, mo))
            .then(|| naive_micros(y, mo, d, h, mi, s, micros, off.unwrap_or(0)))
            .filter(|v| (MIN_DATETIME..=MAX_DATETIME).contains(v));
        match expected {
            Some(v) => prop_assert_eq!(r.map(|doc| doc.get(id(&schema, "f")).cloned()), Ok(Some(Value::Datetime(v))), "{}", text),
            None => prop_assert!(
                matches!(&r, Err(DocumentError::InvalidDatetime { field, .. }) if field == "f"),
                "{}: {:?}", text, r
            ),
        }
    }

    /// Invariant 6, output side: every datetime in the range is written as `YYYY-MM-DDTHH:MM:SS[.ffffff]Z` in
    /// UTC, the fraction exactly when the microseconds are not zero, and the naive calendar reads it back.
    #[test]
    fn datetime_output_form(v in prop_oneof![MIN_DATETIME..=MAX_DATETIME, Just(MIN_DATETIME), Just(MAX_DATETIME), -2_000_000i64..2_000_000]) {
        let schema = all_types(true);
        let doc = Document::new("1").with(id(&schema, "f"), Value::Datetime(v));
        let out: serde_json::Value = serde_json::from_str(&doc.to_json(&schema).unwrap()).unwrap();
        let text = out["f"].as_str().unwrap();
        let has_fraction = v.rem_euclid(1_000_000) != 0;
        prop_assert_eq!(text.len(), if has_fraction { 27 } else { 20 }, "{}", text);
        let b = text.as_bytes();
        for (i, &c) in b.iter().enumerate() {
            let expected_sep = match i {
                4 | 7 => Some(b'-'),
                10 => Some(b'T'),
                13 | 16 => Some(b':'),
                19 if has_fraction => Some(b'.'),
                _ if i == b.len() - 1 => Some(b'Z'),
                _ => None,
            };
            match expected_sep {
                Some(sep) => prop_assert_eq!(c, sep, "{}", text),
                None => prop_assert!(c.is_ascii_digit(), "{}", text),
            }
        }
        let num = |r: std::ops::Range<usize>| text[r].parse::<i64>().unwrap();
        let micros = if has_fraction { num(20..26) } else { 0 };
        prop_assert_eq!(
            naive_micros(num(0..4), num(5..7), num(8..10), num(11..13), num(14..16), num(17..19), micros, 0),
            v
        );
        // And the integer form gives the same document.
        let back = parse(&schema, &format!(r#"{{"id": "1", "f": {v}}}"#)).unwrap();
        prop_assert_eq!(back, doc);
    }

    /// Invariant 5: a stored number's bits survive JSON in, the binary encoding and JSON out, whatever text form
    /// it was written in; an `f64` field keeps them too, except that `-0.0` becomes `0.0`.
    #[test]
    fn number_bits_survive(x in prop_oneof![
        any::<f64>().prop_filter("finite", |x| x.is_finite()),
        Just(-0.0), Just(0.0), Just(5e-324), Just(-5e-324), Just(f64::MAX), Just(f64::MIN), Just(f64::MIN_POSITIVE),
    ]) {
        let lax = all_types(false);
        let (d, g) = (id(&lax, "d"), id(&lax, "g"));
        // Rust's `{:e}` is valid JSON number syntax and independent of serde_json's writer.
        let text = format!(r#"{{"id": "1", "d": {x:e}, "g": [{x:e}], "X": {{"v": {x:e}}}}}"#);
        let doc = parse(&lax, &text).unwrap();
        let field_bits = if x == 0.0 { 0 } else { x.to_bits() };
        let check = |doc: &Document| -> Result<(), TestCaseError> {
            let Some(Value::F64(dv)) = doc.get(d) else { panic!() };
            prop_assert_eq!(dv.to_bits(), field_bits);
            let Some(Value::Json(gv)) = doc.get(g) else { panic!() };
            prop_assert_eq!(gv[0].as_f64().unwrap().to_bits(), x.to_bits());
            prop_assert_eq!(doc.extra()["X"]["v"].as_f64().unwrap().to_bits(), x.to_bits());
            Ok(())
        };
        check(&doc)?;
        let decoded = Document::decode(&doc.encode(&lax, FieldSet::All).unwrap(), &lax).unwrap();
        check(&decoded)?;
        let reparsed = parse(&lax, &decoded.to_json(&lax).unwrap()).unwrap();
        check(&reparsed)?;
    }
}

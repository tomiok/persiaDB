//! Fuzz the binary document encoding (ROADMAP 3.2.3; SPEC §6.4.2, ADR-0023): arbitrary bytes never panic, and an
//! accepted encoding is the canonical one (re-encoding gives the same bytes) and survives a JSON round trip.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::document::{Document, DocumentError, FieldSet};
use persia_engine::schema::{CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions};

fn schema() -> CollectionSchema {
    SchemaBuilder::new("fuzz")
        .field("a", FieldKind::Text(TextOptions::default()))
        .field("b", FieldKind::Keyword(KeywordOptions::default()))
        .field("c", FieldKind::I64 { stored: true })
        .field("d", FieldKind::F64 { stored: true })
        .field("e", FieldKind::Bool { stored: true })
        .field("f", FieldKind::Datetime { stored: true })
        .field("g", FieldKind::Stored)
        .strict(false)
        .build()
        .expect("a valid schema")
}

fuzz_target!(|data: &[u8]| {
    let schema = schema();
    let Ok(doc) = Document::decode(data, &schema) else {
        return;
    };
    let json = doc.to_json(&schema).expect("a decoded document converts to JSON");
    match doc.encode(&schema, FieldSet::All) {
        Ok(bytes) => {
            assert_eq!(bytes, data, "decoding accepted a non-canonical encoding");
            assert_eq!(Document::from_json(&json, &schema).expect("its JSON parses"), doc);
        }
        // A row may hold an extra field named like a field added later (SPEC §6.4.2), which a new document cannot.
        Err(e) => assert!(matches!(e, DocumentError::ExtraIsSchemaField { .. }), "{e:?}"),
    }
});

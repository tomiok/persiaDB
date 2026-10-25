//! Fuzz the JSON document form (ROADMAP 3.2.4; SPEC §6.4.1): arbitrary input never panics, and an accepted document
//! round-trips through JSON and the binary encoding unchanged.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::document::{Document, FieldSet};
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};

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
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let schema = schema();
    let Ok(doc) = Document::from_json(text, &schema) else {
        return;
    };
    let json = doc
        .to_json(&schema)
        .expect("a valid document converts to JSON");
    assert_eq!(
        Document::from_json(&json, &schema).expect("its JSON parses"),
        doc
    );
    let bytes = doc
        .encode(&schema, FieldSet::All)
        .expect("a valid document encodes");
    assert_eq!(
        Document::decode(&bytes, &schema).expect("its encoding decodes"),
        doc
    );
});

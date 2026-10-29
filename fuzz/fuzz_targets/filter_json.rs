//! Fuzz the filter DSL (ROADMAP 7.1.3; SPEC §7.1.2): arbitrary input never panics, and an accepted filter validates
//! against the schema and limits it was parsed with.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::query::{Filter, QueryLimits};
use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};

fuzz_target!(|data: &[u8]| {
    let Ok(json) = std::str::from_utf8(data) else {
        return;
    };
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .field("price", FieldKind::F64 { stored: true })
        .field("stock", FieldKind::I64 { stored: true })
        .field("active", FieldKind::Bool { stored: true })
        .field("at", FieldKind::Datetime { stored: true })
        .field("notes", FieldKind::Stored)
        .build()
        .expect("a valid schema");
    let limits = QueryLimits::default();
    if let Ok(filter) = Filter::from_json(json, &schema, &limits) {
        assert_eq!(filter.validate(&schema, &limits), Ok(()));
    }
});

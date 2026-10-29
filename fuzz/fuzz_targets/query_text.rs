//! Fuzz the user text compiler (ROADMAP 7.1.2.2; SPEC §7.1.1): any text compiles without panicking, and the query
//! validates against the schema and the limits it was compiled with. The first byte picks small limits, so cutting
//! at each limit is reached too.
#![no_main]

use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use persia_analysis::AnalyzerRegistry;
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};

/// Built once: the registry builds every built-in analyzer.
static SETUP: LazyLock<(CollectionSchema, QueryCompiler)> = LazyLock::new(|| {
    let schema = SchemaBuilder::new("c")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                positions: true,
                boost: 3.0,
                ..TextOptions::default()
            }),
        )
        .field(
            "body",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("es").expect("built-in"),
                ..TextOptions::default()
            }),
        )
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .field("price", FieldKind::F64 { stored: true })
        .build()
        .expect("a valid schema");
    let compiler =
        QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).expect("built-in analyzers");
    (schema, compiler)
});

fuzz_target!(|data: &[u8]| {
    let Some((&knobs, text)) = data.split_first() else {
        return;
    };
    let text = String::from_utf8_lossy(text);
    let (schema, compiler) = &*SETUP;
    let knob = |shift: u8| usize::from((knobs >> shift) & 0b11) + 1;
    let limits = if knobs & 1 == 0 {
        QueryLimits::default()
    } else {
        QueryLimits {
            max_terms: knob(1) * 2,
            max_clauses: knob(3) * 2,
            max_depth: knob(5),
            max_text_len: usize::from(knobs) * 4,
            ..QueryLimits::default()
        }
    };
    let compiled = compiler.compile(&text, &DefaultFields::all(schema), &limits);
    assert_eq!(compiled.query.validate(schema, &limits), Ok(()), "{text:?}");
});

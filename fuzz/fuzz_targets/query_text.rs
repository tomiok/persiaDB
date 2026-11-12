//! Fuzz the user text compiler (ROADMAP 7.1.2.2; SPEC §7.1.1): any text compiles without panicking, the query
//! validates against the schema and the limits it was compiled with, and each clause's span indexes the text read
//! (ROADMAP 8.1.7), and the synonym runs it finds (ROADMAP 8.2.1, SPEC §6.3.2) are ordered, of required clauses, and
//! bounded. The first byte picks small limits, so cutting at each limit is reached too.
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
        // Short and common words, several tokens, stopwords and accents, so fuzzed text matches them often.
        .synonyms(
            [
                &["a", "b", "c d"][..],
                &["a b", "e-mail", "la casa"],
                &["the a", "año", "x y z"],
                &["b", "a"],
            ]
            .iter()
            .map(|g| g.iter().map(|&t| t.to_owned()).collect())
            .collect(),
        )
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
    // ROADMAP 8.1.7: each clause's span indexes the text read, a prefix of the text, in order, on character boundaries.
    assert!(text.starts_with(compiled.text.as_str()), "{text:?}");
    let mut end = 0;
    for clause in &compiled.clauses {
        let span = clause.span.clone();
        assert!(
            end <= span.start && span.start < span.end,
            "{text:?}: {span:?}"
        );
        assert!(
            compiled.text.get(span.clone()).is_some(),
            "{text:?}: {span:?}"
        );
        end = span.end;
    }
    // ROADMAP 8.2.1: runs of required clauses, in order and apart, each with 1 to 50 valid alternatives, `max_terms` in all.
    let mut end = 0;
    let mut total = 0usize;
    for run in &compiled.synonyms {
        let clauses = compiled.clauses.get(run.clauses.clone());
        assert!(
            end <= run.clauses.start && run.clauses.start < run.clauses.end,
            "{text:?}"
        );
        assert!(
            clauses.is_some_and(|c| c.iter().all(|c| !c.negated)),
            "{text:?}"
        );
        assert!((1..=50).contains(&run.alternatives.len()), "{text:?}");
        for alternative in &run.alternatives {
            assert_eq!(
                alternative.validate(schema, &QueryLimits::default()),
                Ok(()),
                "{text:?}"
            );
        }
        end = run.clauses.end;
        total += run.alternatives.len();
    }
    assert!(total <= limits.max_terms, "{text:?}");
});

//! The `tracing` event of a relaxed search (SPEC §12.4; ROADMAP 8.3.3), alone in its test binary: a scoped subscriber
//! may miss a callsite that a test on another thread of the same binary registered first.
//!
//! Invariant: a relaxed search states what it did as one `tracing` debug event (its collection, level, applied, hits
//! and truncated); one that does not relax states none.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, missing_docs)] // Tests: inputs are fixed.

use std::sync::{Arc, Mutex};

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{Database, DatabaseOptions, Execution, SearchRequest, Threads};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::relax::{RelaxOptions, RelaxPlan};
use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

/// A subscriber that keeps each event's fields, rendered as `name=value`.
#[derive(Default)]
struct Events(Mutex<Vec<Vec<String>>>);

struct Fields<'a>(&'a mut Vec<String>);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={value:?}", field.name()));
    }
}

impl tracing::Subscriber for Events {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::DEBUG {
            let mut fields = Vec::new();
            event.record(&mut Fields(&mut fields));
            self.0.lock().unwrap().push(fields);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn a_relaxed_search_states_what_it_did_as_one_debug_event() {
    let schema = SchemaBuilder::new("a")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [3; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    for (id, text) in [
        ("1", "red shoes"),
        ("2", "red hat"),
        ("3", "blue running shoes"),
    ] {
        let doc = Document::new(id).with(body, Value::String(text.into()));
        db.put("a", &doc, Durability::Buffered).unwrap();
    }
    let snapshot = db.snapshot();
    let a = snapshot.collection("a").unwrap();
    let compiler = QueryCompiler::new(a.schema(), &AnalyzerRegistry::builtin()).unwrap();
    let memtable = a.memtable();
    let views = a.views(&memtable).unwrap();
    let relax = RelaxPlan::new(a.schema().relaxation(), &RelaxOptions::default()).unwrap();
    // On the calling thread, where the scoped subscriber is.
    let exec = Execution::default().threads(Threads::Current);
    let search = |text: &str| {
        let fields = DefaultFields::all(a.schema());
        let query = compiler.compile(text, &fields, &QueryLimits::default());
        let request = SearchRequest::default();
        views
            .search_relaxed(&query, &request, &relax, &exec)
            .unwrap()
    };
    let events = Arc::new(Events::default());
    tracing::subscriber::with_default(Arc::clone(&events), || {
        // L0 has hits: no event. `shoez` relaxes to L4 (one edit from `shoes`), which finds 2.
        search("red");
        search("shoez");
    });
    let events = events.0.lock().unwrap();
    let relaxed: Vec<&Vec<String>> = events
        .iter()
        .filter(|e| e.iter().any(|f| f == "message=relaxed search"))
        .collect();
    assert_eq!(relaxed.len(), 1, "{events:?}");
    let fields = relaxed[0];
    for want in [
        "collection=\"a\"",
        "level=\"fuzzy\"",
        "hits=2",
        "truncated=false",
    ] {
        assert!(fields.iter().any(|f| f == want), "{want} in {fields:?}");
    }
    assert!(
        fields
            .iter()
            .any(|f| f.starts_with("applied=") && f.contains("fuzzy")),
        "{fields:?}"
    );
}

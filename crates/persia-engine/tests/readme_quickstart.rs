//! The README quickstart ("Index documents and search them"), as a test so the snippet keeps compiling and
//! keeps returning what the README says. Only the path differs: the README writes `app.persia` in the working
//! directory, this writes it in a temporary one, and the typo check is not in the README. Code between
//! `// readme: hidden` and `// readme: shown` is the test's only; the rest must be the README block exactly, up to
//! whitespace and comments (`repo_checks::readme_quickstart_is_the_tested_one`).

// readme: hidden
// `.map(|f| f.id())` is the README's spelling, kept so the two read the same.
#![allow(missing_docs, clippy::redundant_closure_for_method_calls)]

// readme: shown
use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{Database, DatabaseOptions, SearchRequest};
use persia_engine::document::{Document, Value};
use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::wal::Durability;

// readme: hidden
#[test]
// readme: shown
fn readme_quickstart() -> Result<(), Box<dyn std::error::Error>> {
    // readme: hidden
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("app.persia");
    // readme: shown

    let schema = SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()?;
    let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;

    let mut db = Database::create(&path, DatabaseOptions::default())?;
    db.create_collection(schema)?;
    for (id, text) in [
        ("n1", "Hello, world"),
        ("n2", "hello, hello again"),
        ("n3", "goodbye"),
    ] {
        let doc = Document::new(id).with(body, Value::String(text.into()));
        db.put("notes", &doc, Durability::Fsync)?;
    }
    db.commit()?;

    let snapshot = db.snapshot();
    let notes = snapshot.collection("notes").ok_or("no collection")?;
    let compiler = QueryCompiler::new(notes.schema(), &AnalyzerRegistry::builtin())?;
    let query = compiler
        .compile(
            "hello",
            &DefaultFields::all(notes.schema()),
            &QueryLimits::default(),
        )
        .query;
    let memtable = notes.memtable();
    let plan = SearchPlan::new(&query, notes.schema(), &notes.search_statistics(&memtable))?;
    let page =
        notes
            .views(&memtable)?
            .search(&plan, &SearchRequest::default(), Pruning::default())?;

    let ids: Vec<_> = page.hits.iter().map(|h| h.document.id()).collect();
    assert_eq!(ids, ["n2", "n1"]);

    // readme: hidden
    // The README's claim that a typo returns nothing until relaxation (M8) exists.
    let typo = compiler
        .compile(
            "helo",
            &DefaultFields::all(notes.schema()),
            &QueryLimits::default(),
        )
        .query;
    let plan = SearchPlan::new(&typo, notes.schema(), &notes.search_statistics(&memtable))?;
    let page =
        notes
            .views(&memtable)?
            .search(&plan, &SearchRequest::default(), Pruning::default())?;
    assert_eq!(page.hits.len(), 0);
    // readme: shown
    Ok(())
}

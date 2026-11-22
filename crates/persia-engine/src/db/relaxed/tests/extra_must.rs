//! A required clause a caller adds beside a compiled query's clauses (`Compiled::with_query`, ROADMAP 8.2.15, SPEC
//! §7.1.1, §7.6): the compilation stays attached, and every step of every level carries the clause unchanged, so no
//! hit of any level lacks it. `effective` renders the clauses alone: the added one has no source text.

use persia_analysis::AnalyzerRegistry;
use persia_storage::container::sim::SimFile;

use super::exec;
use crate::db::{CollectionViews, Database, DatabaseOptions, SearchRequest};
use crate::document::{Document, Value};
use crate::query::{
    BoolQuery, Compiled, DefaultFields, Filter, FilterValue, Query, QueryCompiler, QueryLimits,
    TermQuery,
};
use crate::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use crate::schema::{
    Fallback, FieldId, FieldKind, FilterRelaxation, KeywordOptions, Level, Levels,
    RelaxationConfig, SchemaBuilder, TextOptions,
};
use crate::search::Projection;
use crate::wal::Durability;

/// `body` text with the synonyms `tv` / `television`, a `brand` the added clause requires, and a `kind` to filter on.
fn database() -> (Database<SimFile>, FieldId, FieldId) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .field("kind", FieldKind::Keyword(KeywordOptions::default()))
        .synonyms(vec![vec!["tv".into(), "television".into()]])
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let brand = schema.field("brand").unwrap().id();
    let kind = schema.field("kind").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    // Each level's words match an `acme` document and a `nike` one.
    let docs = [
        ("a", "red shoes", "nike"),
        ("b", "television stand", "acme"),
        ("c", "wonderful", "nike"),
        ("d", "running jacket", "acme"),
        ("g", "television stand", "nike"),
        ("h", "red hat", "acme"),
        ("i", "running jackets", "nike"),
        ("j", "wonderful", "acme"),
    ];
    for (i, (id, text, b)) in docs.into_iter().enumerate() {
        let doc = Document::new(id)
            .with(body, Value::String(text.into()))
            .with(brand, Value::String(b.into()))
            .with(kind, Value::String("x".into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i == 3 {
            db.commit().unwrap();
        }
    }
    (db, brand, kind)
}

fn compile(views: &CollectionViews<'_>, text: &str) -> Compiled {
    let schema = views.collection().schema();
    QueryCompiler::new(schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(text, &DefaultFields::all(schema), &QueryLimits::default())
}

fn keyword(field: FieldId, value: &str) -> Query {
    Query::Term(TermQuery {
        field,
        term: value.into(),
    })
}

/// `compiled` with `extra` required after its clauses, as a caller puts it in ([`Compiled::with_query`]).
fn with_extra(compiled: Compiled, extra: Query) -> Compiled {
    let query = match compiled.query() {
        Query::Bool(b) if b.should.is_empty() && b.minimum_should_match == 0 => {
            let mut b = b.clone();
            b.must.push(extra);
            Query::Bool(b)
        }
        other => Query::Bool(BoolQuery {
            must: vec![other.clone(), extra],
            ..BoolQuery::default()
        }),
    };
    compiled.with_query(query)
}

fn plan(levels: Vec<Level>, fallback: Fallback, filters: FilterRelaxation) -> RelaxPlan {
    let options = RelaxOptions {
        levels: Some(Levels::Custom(levels)),
        fallback: Some(fallback),
        filters: Some(filters),
        min_results: Some(10),
        ..RelaxOptions::default()
    };
    RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap()
}

fn request() -> SearchRequest {
    SearchRequest {
        projection: Projection::None,
        ..SearchRequest::default()
    }
}

/// The ids of `out`'s hits, sorted, each checked to be of `level`.
fn ids(out: &crate::db::RelaxedResults, level: RelaxLevel) -> Vec<String> {
    let mut ids: Vec<String> = out
        .hits
        .iter()
        .inspect(|h| assert_eq!(h.level, level))
        .map(|h| h.hit.document.id().to_owned())
        .collect();
    ids.sort();
    ids
}

/// A case: the text, the plan, the level of the hits after L0, their ids without and with the clause, `effective`.
type Case<'a> = (
    &'a str,
    RelaxPlan,
    RelaxLevel,
    &'a [&'a str],
    &'a [&'a str],
    &'a str,
);

/// Per word level (and L5), a query whose level finds an `acme` and a `nike` document without the added clause: with
/// it, every level's hits are the `acme` ones alone, the compilation stays attached (the level builds its step), and
/// the response's `effective` is the clauses' text.
#[test]
fn every_level_keeps_a_required_clause_added_beside_the_clauses() {
    let (db, brand, _) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let acme = keyword(brand, "acme");
    let (never, none) = (FilterRelaxation::Never, Fallback::None);
    let cases: Vec<Case<'_>> = vec![
        (
            "tv stand",
            plan(vec![Level::Synonyms], none, never),
            RelaxLevel::Synonyms,
            &["b", "g"],
            &["b"],
            "tv stand",
        ),
        (
            "red purple",
            plan(vec![Level::DropTerms], none, never),
            RelaxLevel::DropTerms,
            &["a", "h"],
            &["h"],
            "red",
        ),
        (
            "running jack",
            plan(vec![Level::Prefix], none, never),
            RelaxLevel::Prefix,
            &["d", "i"],
            &["d"],
            "running jack",
        ),
        (
            "wonderfull",
            plan(vec![Level::Fuzzy], none, never),
            RelaxLevel::Fuzzy,
            &["c", "j"],
            &["j"],
            "wonderful",
        ),
        (
            "zzzz",
            plan(Vec::new(), Fallback::StaticRank, never),
            RelaxLevel::Fallback,
            &["a", "b", "c", "d", "g", "h", "i", "j"],
            &["b", "d", "h", "j"],
            "",
        ),
    ];
    for (text, relax, level, without, with, effective) in cases {
        let compiled = compile(&views, text);
        let out = views
            .search_relaxed(&compiled, &request(), &relax, &exec())
            .unwrap();
        assert_eq!(ids(&out, level), without, "{text} without the clause");
        let extra = with_extra(compiled, acme.clone());
        assert_eq!(extra.extra_must(), std::slice::from_ref(&acme), "{text}");
        assert!(extra.parts(0).is_some(), "{text}: detached");
        let out = views
            .search_relaxed(&extra, &request(), &relax, &exec())
            .unwrap();
        assert_eq!(ids(&out, level), with, "{text} with the clause");
        assert_eq!(
            out.relaxation.effective.as_deref(),
            Some(effective),
            "{text}"
        );
    }
}

/// The filter step keeps the added clause too: no document is of kind `y`, so every earlier step (L5 included) finds
/// nothing; without the filter `red` matches `a` and `h`, and with the clause `h` alone.
#[test]
fn the_filter_step_keeps_a_required_clause_added_beside_the_clauses() {
    let (db, brand, kind) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let acme = keyword(brand, "acme");
    let y = Filter::Term {
        field: kind,
        value: FilterValue::Keyword("y".into()),
    };
    let relax = plan(
        Vec::new(),
        Fallback::StaticRank,
        FilterRelaxation::LastResort,
    );
    let compiled = compile(&views, "red").with_filter([y]);
    let out = views
        .search_relaxed(&compiled, &request(), &relax, &exec())
        .unwrap();
    assert_eq!(ids(&out, RelaxLevel::Filters), ["a", "h"]);
    let extra = with_extra(compiled, acme.clone());
    assert_eq!(extra.extra_must(), std::slice::from_ref(&acme));
    let out = views
        .search_relaxed(&extra, &request(), &relax, &exec())
        .unwrap();
    assert_eq!(ids(&out, RelaxLevel::Filters), ["h"]);
    assert_eq!(out.relaxation.effective.as_deref(), Some("red"));
}

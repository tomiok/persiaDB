//! Every level renders its steps' `effective` for a compiled query (SPEC §7.6, ROADMAP 8.1.14): each step a level
//! builds from a query compiled from `q` states its text, rendered from the clauses' spans (ROADMAP 8.1.7), tier
//! steps included, so the response's `effective` is never absent for a compiled query, whichever step is the hit
//! step.

use persia_analysis::AnalyzerRegistry;
use persia_storage::container::sim::SimFile;

use super::exec;
use crate::db::{CollectionViews, Database, DatabaseOptions, SearchRequest};
use crate::document::{Document, Value};
use crate::query::{Compiled, DefaultFields, Filter, FilterValue, QueryCompiler, QueryLimits};
use crate::relax::{LevelInput, RelaxLevel, RelaxOptions, RelaxPlan};
use crate::schema::{
    Fallback, FieldId, FieldKind, FilterRelaxation, KeywordOptions, Level, Levels,
    RelaxationConfig, SchemaBuilder, TextOptions,
};
use crate::search::Projection;
use crate::wal::Durability;

/// `body` text with the synonyms `tv` / `television`, and a `brand` keyword to filter on.
fn database() -> (Database<SimFile>, FieldId) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .synonyms(vec![vec!["tv".into(), "television".into()]])
        .build()
        .unwrap();
    let brand = schema.field("brand").unwrap().id();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    let docs = [
        ("a", "red shoes", "nike"),
        ("b", "television stand", "acme"),
        ("c", "wonderful", "nike"),
        ("d", "running jacket", "acme"),
        ("e", "amazingly", "acme"),
    ];
    for (i, (id, text, b)) in docs.into_iter().enumerate() {
        let doc = Document::new(id)
            .with(body, Value::String(text.into()))
            .with(brand, Value::String(b.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i == 2 {
            db.commit().unwrap();
        }
    }
    (db, brand)
}

fn compile(views: &CollectionViews<'_>, text: &str) -> Compiled {
    let schema = views.collection().schema();
    QueryCompiler::new(schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(text, &DefaultFields::all(schema), &QueryLimits::default())
}

fn brand(field: FieldId, value: &str) -> Filter {
    Filter::Term {
        field,
        value: FilterValue::Keyword(value.into()),
    }
}

/// L0 and `levels`, with or without L5, with or without the filter step.
fn plan(levels: Vec<Level>, fallback: Fallback, filters: FilterRelaxation) -> RelaxPlan {
    let options = RelaxOptions {
        levels: Some(Levels::Custom(levels)),
        fallback: Some(fallback),
        filters: Some(filters),
        ..RelaxOptions::default()
    };
    RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap()
}

/// Every step every level builds for a query that each of them relaxes (L1's run `tv`, three terms for L2, the last
/// word `jack` and the long word `wonderfull` for the tier steps of L3 and L4, a filter for the filter step) states a
/// text: none leaves `effective` absent.
#[test]
fn every_step_of_every_level_states_its_text_for_a_compiled_query() {
    let (db, brand_field) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let stats = c.search_statistics(&memtable);
    let exec = exec();
    let terms = exec.terms(&views);
    let expands = |field: FieldId| views.expands(field);
    let compiled =
        compile(&views, "tv wonderfull jack -red").with_soft_filter([brand(brand_field, "acme")]);
    let all = plan(
        Level::ALL.to_vec(),
        Fallback::StaticRank,
        FilterRelaxation::LastResort,
    );
    let mut applied = Vec::new();
    for planned in all.planned() {
        // L3's tier step is built only when L2's last rung was not (ROADMAP 8.2.7): ask for it.
        let input = LevelInput::new(compiled.query(), Some(&compiled), &stats)
            .expanding(&expands)
            .dropping(false)
            .walking(&terms)
            .softening(compiled.soft_filter());
        let steps: Vec<_> = planned
            .transform
            .steps(&input)
            .unwrap()
            .into_iter()
            .map(|s| (s.applied, s.effective))
            .collect();
        assert!(!steps.is_empty(), "{:?} built no step", planned.level);
        for (label, effective) in steps {
            assert!(
                effective.is_some(),
                "{:?} step {label:?} has no text",
                planned.level
            );
            applied.push(label);
        }
    }
    // Every level built, the tier steps of L3 and L4 included.
    assert_eq!(
        applied,
        [
            "",
            "synonyms",
            "drop_terms:75%",
            "drop_terms:50%",
            "prefix:last",
            "prefix",
            "fuzzy:1",
            "fuzzy",
            "fallback",
            "filters",
        ]
    );
}

/// Each level, as the hit step, gives the response its text: the response's `effective` is never absent for a compiled
/// query (SPEC §7.6's rendering per level).
#[test]
fn each_level_as_the_hit_step_states_its_text() {
    let (db, brand_field) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let request = SearchRequest {
        projection: Projection::None,
        ..SearchRequest::default()
    };
    let never = FilterRelaxation::Never;
    let none = Fallback::None;
    // (the text, the plan, the hit step's `applied`, the level and text the response states).
    let cases: Vec<(&str, RelaxPlan, &str, RelaxLevel, &str)> = vec![
        (
            "red  shoes",
            plan(Level::ALL.to_vec(), none, never),
            "",
            RelaxLevel::Exact,
            "red shoes",
        ),
        (
            "tv stand",
            plan(vec![Level::Synonyms], none, never),
            "synonyms",
            RelaxLevel::Synonyms,
            "tv stand",
        ),
        (
            "red purple -blue",
            plan(vec![Level::DropTerms], none, never),
            "drop_terms:75%",
            RelaxLevel::DropTerms,
            "red -blue",
        ),
        (
            "running jack",
            plan(vec![Level::Prefix], none, never),
            "prefix:last",
            RelaxLevel::Prefix,
            "running jack",
        ),
        (
            "runn jack",
            plan(vec![Level::Prefix], none, never),
            "prefix",
            RelaxLevel::Prefix,
            "runn jack",
        ),
        (
            "wonderfull",
            plan(vec![Level::Fuzzy], none, never),
            "fuzzy:1",
            RelaxLevel::Fuzzy,
            "wonderful",
        ),
        (
            "amazinglee",
            plan(vec![Level::Fuzzy], none, never),
            "fuzzy",
            RelaxLevel::Fuzzy,
            "amazingly",
        ),
        (
            "zzzz -red",
            plan(Vec::new(), Fallback::StaticRank, never),
            "fallback",
            RelaxLevel::Fallback,
            "-red",
        ),
        (
            "zzzz",
            plan(Vec::new(), Fallback::StaticRank, never),
            "fallback",
            RelaxLevel::Fallback,
            "",
        ),
    ];
    for (text, relax, applied, level, effective) in cases {
        let compiled = compile(&views, text);
        let out = views
            .search_relaxed(&compiled, &request, &relax, &exec())
            .unwrap();
        let hit = out.hits.first().unwrap_or_else(|| panic!("{text}: no hit"));
        let label = hit
            .step
            .checked_sub(1)
            .map_or("", |i| out.relaxation.applied[i].as_str());
        assert_eq!((label, hit.level), (applied, level), "{text}");
        assert_eq!(
            out.relaxation.effective.as_deref(),
            Some(effective),
            "{text}"
        );
    }

    // The filter step: `red` holds only for `nike`, so with `brand: acme` every earlier step finds nothing.
    let compiled = compile(&views, "red").with_soft_filter([brand(brand_field, "acme")]);
    let relax = plan(Vec::new(), none, FilterRelaxation::LastResort);
    let out = views
        .search_relaxed(&compiled, &request, &relax, &exec())
        .unwrap();
    assert_eq!(out.hits.first().map(|h| h.level), Some(RelaxLevel::Filters));
    assert_eq!(out.relaxation.effective.as_deref(), Some("red"));
}

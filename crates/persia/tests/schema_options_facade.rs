//! The `#[non_exhaustive]` schema types re-exported by `persia` are built with their setters and matched with a
//! wildcard, with names from `persia` alone (ROADMAP 10.1.16), as a program that depends only on `persia` would.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use persia::{
    AnalyzerRef, Db, Fallback, FieldKind, FieldType, FilterRelaxation, KeywordOptions, Level,
    Levels, RankFn, RankingConfig, RelaxationConfig, SchemaBuilder, StaticRank, TextOptions,
};

/// A field type's name the way a program outside the engine has to match it: with a wildcard arm.
fn kind_name(t: FieldType) -> &'static str {
    match t {
        FieldType::Text => "text",
        FieldType::Keyword => "keyword",
        _ => "other",
    }
}

#[test]
fn a_schema_built_with_the_setters_round_trips_through_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let title = TextOptions::default()
        .analyzer(AnalyzerRef::builtin("en").unwrap())
        .boost(3.0)
        .positions(true)
        .k1(1.5)
        .b(0.5);
    let relax = RelaxationConfig::default()
        .min_results(3)
        .levels(Levels::Custom(vec![Level::DropTerms]))
        .fallback(Fallback::None)
        .fill(true)
        .filters(FilterRelaxation::LastResort)
        .budget_ms(25);
    let rank = StaticRank::new("sales")
        .weight(0.5)
        .function(RankFn::Linear);
    let schema = SchemaBuilder::new("products")
        .field("title", FieldKind::Text(title.clone()))
        .field(
            "brand",
            FieldKind::Keyword(KeywordOptions::default().facet(true).stored(false)),
        )
        .field("sales", FieldKind::I64 { stored: true })
        .ranking(RankingConfig::default().static_rank(Some(rank.clone())))
        .relaxation(relax.clone())
        .build()
        .unwrap();
    let products = db.create_collection(schema).unwrap();
    let stored = products.schema().unwrap();
    assert_eq!(
        stored.field("title").map(persia::Field::kind),
        Some(&FieldKind::Text(title))
    );
    let brand = stored.field("brand").unwrap().kind();
    assert_eq!(
        brand,
        &FieldKind::Keyword(KeywordOptions::default().facet(true).stored(false))
    );
    assert_eq!(stored.relaxation(), &relax);
    assert_eq!(stored.ranking().static_rank.as_ref(), Some(&rank));
    let names: Vec<_> = stored
        .fields()
        .iter()
        .map(|f| kind_name(f.field_type()))
        .collect();
    // brand, sales, title: the schema lists its fields by name.
    assert_eq!(names, ["keyword", "other", "text"]);
}

//! Queries: the AST and its validation, the user text compiler, the filter DSL and the limits (SPEC §7.1,
//! ROADMAP 7.1).

// Helpers outside #[test] functions are test code too.
#![allow(clippy::unwrap_used, clippy::panic, clippy::too_many_lines)]

use std::fmt::Write as _;
use std::ops::Bound;

use persia_analysis::AnalyzerRegistry;
use persia_engine::query::{
    BoolQuery, BoostQuery, CombinedQuery, Compiled, ConstantScoreQuery, DEFAULT_MAX_EXPANSIONS,
    DefaultFields, Filter, FilterValue, FuzzyQuery, MAX_FILTER_JSON_LEN, PhraseQuery, PhraseTerm,
    PrefixQuery, Query, QueryCompiler, QueryError, QueryLimits, TermQuery, WeightedTerm,
};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldId, FieldKind, FieldType, KeywordOptions, SchemaBuilder,
    TextOptions,
};
use persia_engine::segment::MAX_TERM_LEN;
use proptest::prelude::*;

/// `title` (standard, positions, boost 3), `body` (en, no positions), `brand` keyword, and one field of every
/// other type.
fn schema() -> CollectionSchema {
    SchemaBuilder::new("products")
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
                analyzer: AnalyzerRef::builtin("en").unwrap(),
                ..TextOptions::default()
            }),
        )
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .field("price", FieldKind::F64 { stored: true })
        .field("stock", FieldKind::I64 { stored: true })
        .field("active", FieldKind::Bool { stored: true })
        .field("at", FieldKind::Datetime { stored: true })
        .field("notes", FieldKind::Stored)
        .build()
        .unwrap()
}

fn id(schema: &CollectionSchema, name: &str) -> FieldId {
    schema.field(name).unwrap().id()
}

fn compiler(schema: &CollectionSchema) -> QueryCompiler {
    QueryCompiler::new(schema, &AnalyzerRegistry::builtin()).unwrap()
}

/// A compact rendering: `t:x` term, `(t:x^3|b:x^1)` combined, `t:"a b@2"` phrase, `{+must ?should -not #filter}`.
fn show(schema: &CollectionSchema, q: &Query) -> String {
    let name = |f: FieldId| schema.field_by_id(f).unwrap().name().to_owned();
    match q {
        Query::Term(t) => format!("{}:{}", name(t.field), t.term),
        Query::Combined(c) => {
            let parts: Vec<String> = c
                .fields
                .iter()
                .map(|w| format!("{}:{}^{}", name(w.field), w.term, w.weight))
                .collect();
            format!("({})", parts.join("|"))
        }
        Query::Phrase(p) => {
            let mut out = format!("{}:\"", name(p.field));
            for (i, t) in p.terms.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write!(out, "{}@{}", t.term, t.position).unwrap();
            }
            out + "\""
        }
        Query::Prefix(p) => format!("{}:{}*", name(p.field), p.prefix),
        Query::Fuzzy(f) => format!("{}:{}~{}", name(f.field), f.term, f.distance),
        Query::Bool(b) => {
            let mut parts = Vec::new();
            parts.extend(b.must.iter().map(|q| format!("+{}", show(schema, q))));
            parts.extend(b.should.iter().map(|q| format!("?{}", show(schema, q))));
            parts.extend(b.must_not.iter().map(|q| format!("-{}", show(schema, q))));
            parts.extend(b.filter.iter().map(|f| format!("#{f:?}")));
            format!("{{{}}}", parts.join(" "))
        }
        Query::Boost(b) => format!("{}^{}", show(schema, &b.query), b.boost),
        Query::ConstantScore(c) => format!("const({})={}", show(schema, &c.query), c.score),
        Query::MatchAll => "*".to_owned(),
        other => format!("{other:?}"),
    }
}

fn compile_with(text: &str, fields: &DefaultFields, limits: &QueryLimits) -> (String, bool) {
    let schema = schema();
    let Compiled { query, truncated } = compiler(&schema).compile(text, fields, limits);
    // Whatever the text, the query is valid.
    query.validate(&schema, limits).unwrap();
    (show(&schema, &query), truncated)
}

fn compile(text: &str) -> String {
    let schema = schema();
    let (shown, truncated) =
        compile_with(text, &DefaultFields::all(&schema), &QueryLimits::default());
    assert!(!truncated, "{text:?}");
    shown
}

#[test]
fn user_text_compiles_as_specified() {
    // (user text, compiled query) — SPEC §7.1.1.
    let cases: &[(&str, &str)] = &[
        // Words: every one required, analyzed per field; `body` (en) stems and drops stopwords.
        ("", "*"),
        ("   \t\n ", "*"),
        ("Shoes", "(body:shoe^1|title:shoes^3)"),
        (
            "red shoes",
            "{+(body:red^1|title:red^3) +(body:shoe^1|title:shoes^3)}",
        ),
        ("the", "(title:the^3)"),
        (
            "e-mail",
            "{+(body:e^1|title:e^3) +(body:mail^1|title:mail^3)}",
        ),
        ("!!! ...", "*"),
        // Phrases: per field, one of which must match; `body` has no positions, so its terms are all required.
        (
            "\"running shoes\"",
            "{?{+body:run +body:shoe} ?title:\"running@0 shoes@1\"}",
        ),
        // A stopword leaves its gap in the phrase.
        (
            "\"shoes for kids\"",
            "{?{+body:shoe +body:kid} ?title:\"shoes@0 for@1 kids@2\"}",
        ),
        // At most one token per field: a word.
        ("\"shoes\"", "(body:shoe^1|title:shoes^3)"),
        ("\"\"", "*"),
        ("\"!!\"", "*"),
        // Exclusion.
        (
            "shoes -red",
            "{+(body:shoe^1|title:shoes^3) -(body:red^1|title:red^3)}",
        ),
        ("-red", "{-(body:red^1|title:red^3)}"),
        (
            "-e-mail",
            "{-{+(body:e^1|title:e^3) +(body:mail^1|title:mail^3)}}",
        ),
        (
            "-\"red shoes\"",
            "{-{?{+body:red +body:shoe} ?title:\"red@0 shoes@1\"}}",
        ),
        ("--red", "{-(body:red^1|title:red^3)}"),
        ("-the", "{-(title:the^3)}"),
        // `field:`: that field only, with terms; a keyword as is.
        ("title:Shoes", "title:shoes"),
        ("body:running", "body:run"),
        ("brand:Nike", "brand:Nike"),
        ("brand:\"New Balance\"", "brand:New Balance"),
        ("-brand:nike", "{-brand:nike}"),
        ("title:\"red shoes\"", "title:\"red@0 shoes@1\""),
        ("body:\"red shoes\"", "{+body:red +body:shoe}"),
        ("title:\"!!\"", "*"),
        ("body:the", "*"),
        // Degraded syntax: plain text.
        (
            "\"red shoes",
            "{+(body:red^1|title:red^3) +(body:shoe^1|title:shoes^3)}",
        ),
        (
            "red \"shoes",
            "{+(body:red^1|title:red^3) +(body:shoe^1|title:shoes^3)}",
        ),
        ("-", "*"),
        ("- red", "(body:red^1|title:red^3)"),
        ("title:", "(body:titl^1|title:title^3)"),
        (
            "title: red",
            "{+(body:titl^1|title:title^3) +(body:red^1|title:red^3)}",
        ),
        (
            "price:10",
            "{+(body:price^1|title:price^3) +(body:10^1|title:10^3)}",
        ),
        ("notes:x", "(body:notes:x^1|title:notes:x^3)"),
        ("color:red", "(body:color:r^1|title:color:red^3)"),
        ("Title:red", "(body:title:r^1|title:title:red^3)"),
        (
            "a\"b c\"",
            "{+(title:a^3) +(body:b^1|title:b^3) +(body:c^1|title:c^3)}",
        ),
        ("\"a\"\"b\"", "{+(title:a^3) +(body:b^1|title:b^3)}"),
    ];
    let mut failures = Vec::new();
    for &(text, expected) in cases {
        let got = compile(text);
        if got != expected {
            failures.push(format!(
                "{text:?}\n   expected {expected}\n        got {got}"
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn default_fields_choose_fields_and_weights() {
    let schema = schema();
    let limits = QueryLimits::default();
    let fields = DefaultFields::parse(&schema, &["body^2"]).unwrap();
    assert_eq!(compile_with("shoes", &fields, &limits).0, "(body:shoe^2)");
    let fields = DefaultFields::parse(&schema, &["body", "title"]).unwrap();
    assert_eq!(
        compile_with("shoes", &fields, &limits).0,
        "(body:shoe^1|title:shoes^3)"
    );
    // A phrase over a field without positions alone: every term required.
    let fields = DefaultFields::parse(&schema, &["body"]).unwrap();
    assert_eq!(
        compile_with("\"red shoes\"", &fields, &limits).0,
        "{+body:red +body:shoe}"
    );
    // A field outside them is still reachable with `field:`.
    assert_eq!(compile_with("title:red", &fields, &limits).0, "title:red");
    // Fields of another schema: only ids that name a `text` field of this one are used.
    let other = SchemaBuilder::new("o")
        .field("a", FieldKind::Text(TextOptions::default()))
        .field("b", FieldKind::Text(TextOptions::default()))
        .field("c", FieldKind::Text(TextOptions::default()))
        .field("d", FieldKind::Text(TextOptions::default()))
        .field("e", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    assert_eq!(
        compile_with("red", &DefaultFields::all(&other), &limits).0,
        "(body:red^1)"
    );
    // No text field: words match nothing to require.
    let keywords = SchemaBuilder::new("k")
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let compiled =
        compiler(&keywords).compile("red brand:x", &DefaultFields::all(&keywords), &limits);
    assert_eq!(show(&keywords, &compiled.query), "brand:x");
}

#[test]
fn default_fields_reject_bad_entries() {
    let schema = schema();
    let err = |specs: &[&str]| DefaultFields::parse(&schema, specs).unwrap_err();
    assert_eq!(
        err(&[]),
        QueryError::InvalidFieldSpec {
            spec: String::new()
        }
    );
    assert_eq!(
        err(&["title^x"]),
        QueryError::InvalidFieldSpec {
            spec: "title^x".into()
        }
    );
    assert_eq!(
        err(&["nope"]),
        QueryError::UnknownField {
            field: "nope".into()
        }
    );
    assert_eq!(
        err(&["brand"]),
        QueryError::WrongFieldType {
            field: "brand".into(),
            field_type: FieldType::Keyword,
            what: "a default field"
        }
    );
    assert_eq!(
        err(&["title", "title^2"]),
        QueryError::DuplicateField {
            field: "title".into()
        }
    );
    for weight in ["0", "-1", "1000.5", "inf", "NaN"] {
        assert!(
            matches!(
                err(&[&format!("title^{weight}")]),
                QueryError::InvalidBoost { .. }
            ),
            "{weight}"
        );
    }
    assert_eq!(
        DefaultFields::parse(&schema, &["title^1000"])
            .unwrap()
            .fields(),
        [(id(&schema, "title"), 1000.0)]
    );
}

#[test]
fn compiler_needs_every_analyzer() {
    let schema = schema();
    let err = QueryCompiler::new(&schema, &AnalyzerRegistry::new()).unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownAnalyzer { ref field, .. } if field == "body"),
        "{err:?}"
    );
}

#[test]
fn user_text_is_cut_to_the_limits() {
    let schema = schema();
    let fields = DefaultFields::parse(&schema, &["title"]).unwrap();
    let limits = |f: fn(&mut QueryLimits)| {
        let mut l = QueryLimits::default();
        f(&mut l);
        l
    };
    // Text: cut at a character boundary.
    let l = limits(|l| l.max_text_len = 5);
    assert_eq!(
        compile_with("red shoes", &fields, &l),
        ("{+(title:red^3) +(title:s^3)}".into(), true)
    );
    assert_eq!(
        compile_with("red", &fields, &l),
        ("(title:red^3)".into(), false)
    );
    let l = limits(|l| l.max_text_len = 1);
    assert_eq!(compile_with("ñ", &fields, &l), ("*".into(), true));
    // Terms: the clause that passes the limit and every later one are left out.
    let l = limits(|l| l.max_terms = 2);
    assert_eq!(
        compile_with("a b c -d", &fields, &l),
        ("{+(title:a^3) +(title:b^3)}".into(), true)
    );
    assert_eq!(
        compile_with("a b", &fields, &l),
        ("{+(title:a^3) +(title:b^3)}".into(), false)
    );
    assert_eq!(compile_with("\"a b c\" d", &fields, &l), ("*".into(), true));
    // Clauses: the root counts as one.
    let l = limits(|l| l.max_clauses = 3);
    assert_eq!(
        compile_with("a b c", &fields, &l),
        ("{+(title:a^3) +(title:b^3)}".into(), true)
    );
    // Depth: a negated multi-token word nests a Bool at depth 2.
    let l = limits(|l| l.max_depth = 2);
    assert_eq!(
        compile_with("a -e-mail b", &fields, &l),
        ("(title:a^3)".into(), true)
    );
    // Limits that do not even admit the root.
    let l = limits(|l| l.max_clauses = 0);
    assert_eq!(
        compile_with(
            "",
            &fields,
            &QueryLimits {
                max_clauses: 1,
                ..l
            }
        )
        .0,
        "*"
    );
    let compiled = compiler(&schema).compile("red", &fields, &l);
    assert_eq!(
        (compiled.query, compiled.truncated),
        (Query::MatchAll, true)
    );
}

fn term(field: FieldId, t: &str) -> Query {
    Query::Term(TermQuery {
        field,
        term: t.into(),
    })
}

#[test]
fn hand_built_queries_validate() {
    let schema = schema();
    let limits = QueryLimits::default();
    let (title, body, brand, price) = (
        id(&schema, "title"),
        id(&schema, "body"),
        id(&schema, "brand"),
        id(&schema, "price"),
    );
    let valid = Query::Bool(BoolQuery {
        must: vec![
            term(brand, "nike"),
            Query::Combined(CombinedQuery {
                fields: vec![
                    WeightedTerm {
                        field: title,
                        term: "x".into(),
                        weight: 3.0,
                    },
                    WeightedTerm {
                        field: body,
                        term: "x".into(),
                        weight: 1.0,
                    },
                ],
            }),
            Query::Phrase(PhraseQuery {
                field: title,
                terms: vec![
                    PhraseTerm {
                        term: "a".into(),
                        position: 0,
                    },
                    PhraseTerm {
                        term: "b".into(),
                        position: 0,
                    },
                    PhraseTerm {
                        term: "c".into(),
                        position: 2,
                    },
                ],
            }),
        ],
        should: vec![
            Query::Prefix(PrefixQuery {
                field: brand,
                prefix: "ni".into(),
                max_expansions: DEFAULT_MAX_EXPANSIONS,
            }),
            Query::Fuzzy(FuzzyQuery {
                field: title,
                term: "shoe".into(),
                distance: 2,
                max_expansions: 1,
            }),
        ],
        must_not: vec![Query::Boost(BoostQuery {
            query: Box::new(Query::ConstantScore(ConstantScoreQuery {
                query: Box::new(Query::MatchAll),
                score: 0.0,
            })),
            boost: 1000.0,
        })],
        filter: vec![Filter::Exists { field: price }],
    });
    valid.validate(&schema, &limits).unwrap();
    assert_eq!(Query::MatchAll.validate(&schema, &limits), Ok(()));
    assert_eq!(
        Query::Bool(BoolQuery::default()).validate(&schema, &limits),
        Ok(())
    );
    // Empty terms are fine (a keyword value may be ""), long ones are not.
    term(brand, "").validate(&schema, &limits).unwrap();
    term(brand, &"x".repeat(MAX_TERM_LEN))
        .validate(&schema, &limits)
        .unwrap();
}

#[test]
fn hand_built_queries_report_each_problem() {
    let schema = schema();
    let limits = QueryLimits::default();
    let (title, body, brand, price) = (
        id(&schema, "title"),
        id(&schema, "body"),
        id(&schema, "brand"),
        id(&schema, "price"),
    );
    let err = |q: Query| q.validate(&schema, &limits).unwrap_err();
    let wrong = |field: &str, field_type, what| QueryError::WrongFieldType {
        field: field.into(),
        field_type,
        what,
    };
    let combined = |fields: Vec<(FieldId, f64)>| {
        Query::Combined(CombinedQuery {
            fields: fields
                .into_iter()
                .map(|(field, weight)| WeightedTerm {
                    field,
                    term: "x".into(),
                    weight,
                })
                .collect(),
        })
    };
    let phrase = |field, positions: &[u32]| {
        Query::Phrase(PhraseQuery {
            field,
            terms: positions
                .iter()
                .map(|&position| PhraseTerm {
                    term: "x".into(),
                    position,
                })
                .collect(),
        })
    };
    let prefix = |field, p: &str, max_expansions| {
        Query::Prefix(PrefixQuery {
            field,
            prefix: p.into(),
            max_expansions,
        })
    };
    let fuzzy = |field, distance, max_expansions| {
        Query::Fuzzy(FuzzyQuery {
            field,
            term: "x".into(),
            distance,
            max_expansions,
        })
    };
    let boost = |boost| {
        Query::Boost(BoostQuery {
            query: Box::new(Query::MatchAll),
            boost,
        })
    };
    let constant = |score| {
        Query::ConstantScore(ConstantScoreQuery {
            query: Box::new(Query::MatchAll),
            score,
        })
    };

    // FieldIds come from a schema; one of a larger schema is unknown here.
    let wide = (0..10)
        .fold(SchemaBuilder::new("w"), |b, i| {
            b.field(
                format!("f{i}"),
                FieldKind::Keyword(KeywordOptions::default()),
            )
        })
        .build()
        .unwrap();
    let unknown = id(&wide, "f9");
    assert_eq!(
        err(term(unknown, "x")),
        QueryError::UnknownFieldId { field: unknown }
    );
    assert_eq!(
        err(term(price, "x")),
        wrong("price", FieldType::F64, "a term query")
    );
    assert_eq!(
        err(term(title, &"x".repeat(MAX_TERM_LEN + 1))),
        QueryError::TermTooLong {
            field: "title".into(),
            len: MAX_TERM_LEN + 1
        }
    );
    assert_eq!(err(combined(vec![])), QueryError::EmptyCombined);
    assert_eq!(
        err(combined(vec![(brand, 1.0)])),
        wrong("brand", FieldType::Keyword, "a combined query")
    );
    assert_eq!(
        err(combined(vec![(title, 1.0), (body, 1.0), (title, 2.0)])),
        QueryError::DuplicateField {
            field: "title".into()
        }
    );
    for weight in [0.0, -1.0, 1000.1, f64::NAN, f64::INFINITY] {
        assert!(matches!(
            err(combined(vec![(title, weight)])),
            QueryError::InvalidBoost { .. }
        ));
        assert!(matches!(
            err(boost(weight)),
            QueryError::InvalidBoost { .. }
        ));
    }
    assert_eq!(
        err(phrase(body, &[0, 1])),
        QueryError::NoPositions {
            field: "body".into()
        }
    );
    assert_eq!(
        err(phrase(brand, &[0])),
        wrong("brand", FieldType::Keyword, "a phrase")
    );
    for positions in [&[][..], &[1, 0]] {
        assert_eq!(
            err(phrase(title, positions)),
            QueryError::InvalidPhrase {
                field: "title".into()
            }
        );
    }
    assert_eq!(
        err(prefix(title, "", 1)),
        QueryError::EmptyPrefix {
            field: "title".into()
        }
    );
    assert_eq!(err(prefix(title, "a", 0)), QueryError::NoExpansions);
    assert_eq!(
        err(prefix(price, "a", 1)),
        wrong("price", FieldType::F64, "a prefix query")
    );
    for distance in [0, 3, u8::MAX] {
        assert_eq!(
            err(fuzzy(title, distance, 1)),
            QueryError::InvalidDistance { distance }
        );
    }
    assert_eq!(err(fuzzy(title, 1, 0)), QueryError::NoExpansions);
    for score in [-0.5, f64::NAN, f64::INFINITY] {
        assert!(matches!(
            err(constant(score)),
            QueryError::InvalidScore { .. }
        ));
    }
    // Problems are found inside Bool, Boost and ConstantScore, and in Bool filters.
    let nested = Query::Boost(BoostQuery {
        query: Box::new(Query::ConstantScore(ConstantScoreQuery {
            query: Box::new(Query::Bool(BoolQuery {
                must_not: vec![term(price, "x")],
                ..BoolQuery::default()
            })),
            score: 1.0,
        })),
        boost: 1.0,
    });
    assert_eq!(err(nested), wrong("price", FieldType::F64, "a term query"));
    let filtered = Query::Bool(BoolQuery {
        filter: vec![Filter::Term {
            field: title,
            value: FilterValue::Keyword("x".into()),
        }],
        ..BoolQuery::default()
    });
    assert_eq!(
        err(filtered),
        wrong("title", FieldType::Text, "a term filter")
    );
}

/// A Bool of `n` term queries.
fn terms_query(field: FieldId, n: usize) -> Query {
    Query::Bool(BoolQuery {
        should: (0..n).map(|i| term(field, &i.to_string())).collect(),
        ..BoolQuery::default()
    })
}

/// `depth` levels: Bools nested around a `MatchAll`.
fn nested(depth: usize) -> Query {
    (1..depth).fold(Query::MatchAll, |q, _| {
        Query::Bool(BoolQuery {
            must: vec![q],
            ..BoolQuery::default()
        })
    })
}

#[test]
fn limits_are_explicit_errors_at_their_bound() {
    let schema = schema();
    let title = id(&schema, "title");
    let limits = QueryLimits {
        max_terms: 4,
        max_clauses: 6,
        max_expansions: 100,
        max_depth: 3,
        max_text_len: 0,
    };
    let check = |q: &Query| q.validate(&schema, &limits);
    // Terms: 4 fit, 5 do not (and need 6 clauses, within the limit).
    assert_eq!(check(&terms_query(title, 4)), Ok(()));
    assert_eq!(
        check(&terms_query(title, 5)),
        Err(QueryError::TooManyTerms { limit: 4 })
    );
    // Clauses: 6 nodes fit, 7 do not.
    let clauses = |n: usize| {
        Query::Bool(BoolQuery {
            must: vec![Query::MatchAll; n - 1],
            ..BoolQuery::default()
        })
    };
    assert_eq!(check(&clauses(6)), Ok(()));
    assert_eq!(
        check(&clauses(7)),
        Err(QueryError::TooManyClauses { limit: 6 })
    );
    // Depth.
    assert_eq!(check(&nested(3)), Ok(()));
    assert_eq!(check(&nested(4)), Err(QueryError::TooDeep { limit: 3 }));
    let boosted = Query::Boost(BoostQuery {
        query: Box::new(nested(3)),
        boost: 1.0,
    });
    assert_eq!(check(&boosted), Err(QueryError::TooDeep { limit: 3 }));
    // Expansions add up over prefix and fuzzy queries.
    let expanding = |a, b| {
        Query::Bool(BoolQuery {
            should: vec![
                Query::Prefix(PrefixQuery {
                    field: title,
                    prefix: "a".into(),
                    max_expansions: a,
                }),
                Query::Fuzzy(FuzzyQuery {
                    field: title,
                    term: "abc".into(),
                    distance: 1,
                    max_expansions: b,
                }),
            ],
            ..BoolQuery::default()
        })
    };
    assert_eq!(check(&expanding(50, 50)), Ok(()));
    assert_eq!(
        check(&expanding(50, 51)),
        Err(QueryError::TooManyExpansions { limit: 100 })
    );
    assert_eq!(
        check(&expanding(u32::MAX, 1)),
        Err(QueryError::TooManyExpansions { limit: 100 })
    );
    // Filter values count as terms; filter nodes as clauses.
    let filtered = |n: usize| {
        Query::Bool(BoolQuery {
            filter: vec![Filter::Terms {
                field: id(&schema, "stock"),
                values: (0..n)
                    .map(|i| FilterValue::I64(i64::try_from(i).unwrap()))
                    .collect(),
            }],
            ..BoolQuery::default()
        })
    };
    assert_eq!(check(&filtered(4)), Ok(()));
    assert_eq!(
        check(&filtered(5)),
        Err(QueryError::TooManyTerms { limit: 4 })
    );
}

#[test]
fn default_limits() {
    let l = QueryLimits::default();
    assert_eq!(
        (
            l.max_terms,
            l.max_clauses,
            l.max_expansions,
            l.max_depth,
            l.max_text_len
        ),
        (1024, 1024, 1024, 32, 8192)
    );
    assert_eq!(DEFAULT_MAX_EXPANSIONS, 50);
}

fn filter(json: &str) -> Result<Filter, QueryError> {
    Filter::from_json(json, &schema(), &QueryLimits::default())
}

#[test]
fn filters_parse_every_form() {
    let schema = schema();
    let f = |name| id(&schema, name);
    let cases: Vec<(&str, Filter)> = vec![
        (
            r#"{"term": ["brand", "nike"]}"#,
            Filter::Term {
                field: f("brand"),
                value: FilterValue::Keyword("nike".into()),
            },
        ),
        (
            r#"{"term": ["price", 10]}"#,
            Filter::Term {
                field: f("price"),
                value: FilterValue::F64(10.0),
            },
        ),
        (
            r#"{"term": ["stock", -3]}"#,
            Filter::Term {
                field: f("stock"),
                value: FilterValue::I64(-3),
            },
        ),
        (
            r#"{"term": ["active", true]}"#,
            Filter::Term {
                field: f("active"),
                value: FilterValue::Bool(true),
            },
        ),
        (
            r#"{"term": ["at", "1970-01-01T00:00:01+01:00"]}"#,
            Filter::Term {
                field: f("at"),
                value: FilterValue::Datetime(-3_599_000_000),
            },
        ),
        (
            r#"{"terms": ["brand", ["a", "b"]]}"#,
            Filter::Terms {
                field: f("brand"),
                values: vec![
                    FilterValue::Keyword("a".into()),
                    FilterValue::Keyword("b".into()),
                ],
            },
        ),
        (
            r#"{"terms": ["stock", []]}"#,
            Filter::Terms {
                field: f("stock"),
                values: vec![],
            },
        ),
        (
            r#"{"range": ["price", {"gt": 1.5, "lte": 100}]}"#,
            Filter::Range {
                field: f("price"),
                lower: Bound::Excluded(FilterValue::F64(1.5)),
                upper: Bound::Included(FilterValue::F64(100.0)),
            },
        ),
        (
            r#"{"range": ["stock", {"gte": 0}]}"#,
            Filter::Range {
                field: f("stock"),
                lower: Bound::Included(FilterValue::I64(0)),
                upper: Bound::Unbounded,
            },
        ),
        (
            r#"{"range": ["at", {"lt": 5}]}"#,
            Filter::Range {
                field: f("at"),
                lower: Bound::Unbounded,
                upper: Bound::Excluded(FilterValue::Datetime(5)),
            },
        ),
        (
            r#"{"exists": "title"}"#,
            Filter::Exists { field: f("title") },
        ),
        (r#"{"and": []}"#, Filter::And(vec![])),
        (
            r#"{"or": [{"exists": "brand"}, {"not": {"term": ["active", false]}}]}"#,
            Filter::Or(vec![
                Filter::Exists { field: f("brand") },
                Filter::Not(Box::new(Filter::Term {
                    field: f("active"),
                    value: FilterValue::Bool(false),
                })),
            ]),
        ),
    ];
    for (json, expected) in cases {
        assert_eq!(filter(json), Ok(expected), "{json}");
    }
}

#[test]
fn filters_reject_bad_input_explicitly() {
    let syntax = |json: &str| match filter(json) {
        Err(QueryError::FilterSyntax(m)) => m,
        other => panic!("{json}: {other:?}"),
    };
    // Not JSON, duplicate keys, not of the DSL's shape.
    for json in [
        "",
        "{",
        "[]",
        "null",
        "{}",
        r#"{"term": ["brand", "a"], "exists": "brand"}"#,
        r#"{"exists": "brand", "exists": "price"}"#,
        r#"{"and": [{"exists": "brand"}], "and": []}"#,
        r#"{"match": "x"}"#,
        r#"{"term": "brand"}"#,
        r#"{"term": ["brand"]}"#,
        r#"{"term": ["brand", "a", "b"]}"#,
        r#"{"term": [1, "a"]}"#,
        r#"{"terms": ["brand", "a"]}"#,
        r#"{"range": ["price", 5]}"#,
        r#"{"range": ["price", {"eq": 5}]}"#,
        r#"{"range": ["price", {"lt": 1, "lt": 2}]}"#,
        r#"{"exists": ["brand"]}"#,
        r#"{"and": {"exists": "brand"}}"#,
        r#"{"not": [{"exists": "brand"}]}"#,
        r#"{"term": ["brand", "a"]} x"#,
    ] {
        syntax(json);
    }
    assert!(syntax(r#"{"exists": "a", "exists": "b"}"#).contains("duplicate key"));
    let long = format!("{{\"exists\": \"{}\"}}", " ".repeat(MAX_FILTER_JSON_LEN));
    assert!(syntax(&long).contains("at most"));
    // Fields.
    assert_eq!(
        filter(r#"{"exists": "nope"}"#),
        Err(QueryError::UnknownField {
            field: "nope".into()
        })
    );
    let wrong = |json: &str| match filter(json) {
        Err(QueryError::WrongFieldType { field, .. }) => field,
        other => panic!("{json}: {other:?}"),
    };
    assert_eq!(wrong(r#"{"term": ["title", "x"]}"#), "title");
    assert_eq!(wrong(r#"{"terms": ["notes", ["x"]]}"#), "notes");
    assert_eq!(wrong(r#"{"range": ["brand", {"gt": "a"}]}"#), "brand");
    assert_eq!(wrong(r#"{"range": ["active", {"gt": true}]}"#), "active");
    assert_eq!(wrong(r#"{"exists": "notes"}"#), "notes");
    // Values of the wrong type or out of range.
    for json in [
        r#"{"term": ["brand", 1]}"#,
        r#"{"term": ["stock", 1.5]}"#,
        r#"{"term": ["stock", 9223372036854775808]}"#,
        r#"{"term": ["stock", "1"]}"#,
        r#"{"term": ["price", "1"]}"#,
        r#"{"term": ["active", 1]}"#,
        r#"{"term": ["at", "2026-01-01"]}"#,
        r#"{"term": ["at", "10000-01-01T00:00:00Z"]}"#,
        r#"{"term": ["at", 253402300800000000]}"#,
        r#"{"term": ["at", 1.0]}"#,
        r#"{"terms": ["brand", ["a", null]]}"#,
        r#"{"range": ["stock", {"gt": 0.5}]}"#,
    ] {
        assert!(
            matches!(filter(json), Err(QueryError::FilterValue { .. })),
            "{json}"
        );
    }
    let too_long = format!(r#"{{"term": ["brand", "{}"]}}"#, "x".repeat(32 * 1024 + 1));
    assert!(matches!(
        filter(&too_long),
        Err(QueryError::FilterValue { .. })
    ));
    // Ranges.
    for (json, problem) in [
        (r#"{"range": ["price", {}]}"#, "needs a bound"),
        (
            r#"{"range": ["price", {"gt": 1, "gte": 2}]}"#,
            "two bounds on one side",
        ),
        (
            r#"{"range": ["price", {"lte": 1, "lt": 2}]}"#,
            "two bounds on one side",
        ),
    ] {
        assert_eq!(
            filter(json),
            Err(QueryError::InvalidRange {
                field: "price".into(),
                problem
            }),
            "{json}"
        );
    }
}

#[test]
fn filters_are_within_the_limits() {
    let schema = schema();
    let limits = QueryLimits {
        max_terms: 3,
        max_depth: 3,
        max_clauses: 5,
        ..QueryLimits::default()
    };
    let parse = |json: &str| Filter::from_json(json, &schema, &limits);
    assert!(parse(r#"{"terms": ["stock", [1, 2, 3]]}"#).is_ok());
    assert_eq!(
        parse(r#"{"terms": ["stock", [1, 2, 3, 4]]}"#),
        Err(QueryError::TooManyTerms { limit: 3 })
    );
    assert!(parse(r#"{"range": ["stock", {"gt": 1, "lt": 2}]}"#).is_ok());
    assert!(parse(r#"{"not": {"not": {"exists": "brand"}}}"#).is_ok());
    assert_eq!(
        parse(r#"{"not": {"not": {"not": {"exists": "brand"}}}}"#),
        Err(QueryError::TooDeep { limit: 3 })
    );
    assert_eq!(
        parse(
            r#"{"or": [{"exists": "brand"}, {"exists": "brand"}, {"exists": "brand"}, {"exists": "brand"}, {"exists": "brand"}]}"#
        ),
        Err(QueryError::TooManyClauses { limit: 5 })
    );
    // JSON nested past serde_json's recursion limit is a syntax error, not a stack overflow.
    let deep = format!(
        "{}{{\"exists\": \"brand\"}}{}",
        "{\"not\": ".repeat(10_000),
        "}".repeat(10_000)
    );
    assert!(matches!(
        Filter::from_json(&deep, &schema, &QueryLimits::default()),
        Err(QueryError::FilterSyntax(_))
    ));
}

#[test]
fn hand_built_filters_validate() {
    let schema = schema();
    let limits = QueryLimits::default();
    let price = id(&schema, "price");
    let check = |f: Filter| f.validate(&schema, &limits);
    assert_eq!(
        check(Filter::Range {
            field: price,
            lower: Bound::Unbounded,
            upper: Bound::Unbounded
        }),
        Err(QueryError::InvalidRange {
            field: "price".into(),
            problem: "needs a bound"
        })
    );
    for value in [FilterValue::F64(f64::NAN), FilterValue::I64(1)] {
        assert!(matches!(
            check(Filter::Term {
                field: price,
                value
            }),
            Err(QueryError::FilterValue { .. })
        ));
    }
    assert!(matches!(
        check(Filter::Term {
            field: id(&schema, "at"),
            value: FilterValue::Datetime(i64::MAX)
        }),
        Err(QueryError::FilterValue { .. })
    ));
    assert_eq!(
        check(Filter::Range {
            field: price,
            lower: Bound::Included(FilterValue::F64(-0.0)),
            upper: Bound::Excluded(FilterValue::F64(f64::INFINITY))
        }),
        Ok(())
    );
}

/// User-ish text: words, quotes, dashes, colons, field names, whitespace and some non-ASCII.
fn user_text() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("\"".to_owned()),
        Just("-".to_owned()),
        Just(":".to_owned()),
        Just(" ".to_owned()),
        Just("\t".to_owned()),
        Just("title:".to_owned()),
        Just("body:".to_owned()),
        Just("brand:".to_owned()),
        Just("price:".to_owned()),
        Just("the".to_owned()),
        "[a-zA-Z0-9]{1,8}",
        "\\PC{1,4}",
    ];
    prop::collection::vec(piece, 0..40).prop_map(|p| p.concat())
}

proptest! {
    /// SPEC §7.1.1: user text never fails, and its query always validates within the limits.
    #[test]
    fn user_text_always_compiles_to_a_valid_query(
        text in prop_oneof![user_text(), any::<String>()],
        max_terms in 1_usize..20,
        max_clauses in 1_usize..20,
        max_depth in 1_usize..6,
        max_text_len in 0_usize..300,
        all_fields in any::<bool>(),
    ) {
        let schema = schema();
        let limits = QueryLimits { max_terms, max_clauses, max_depth, max_text_len, ..QueryLimits::default() };
        let fields = if all_fields {
            DefaultFields::all(&schema)
        } else {
            DefaultFields::parse(&schema, &["body^2"]).unwrap()
        };
        let compiled = compiler(&schema).compile(&text, &fields, &limits);
        prop_assert_eq!(compiled.query.validate(&schema, &limits), Ok(()));
        // Nothing is cut when the limits are generous.
        let generous = compiler(&schema).compile(&text, &fields, &QueryLimits::default());
        prop_assert!(!generous.truncated || text.len() > QueryLimits::default().max_text_len);
        // Within the text length, a compilation cut by the other limits keeps a prefix of the generous one's clauses.
        if !compiled.truncated {
            prop_assert_eq!(&compiled.query, &generous.query);
        } else if text.len() <= max_text_len {
            let ((must, not), (all_must, all_not)) = (clauses(&compiled.query), clauses(&generous.query));
            let collapsed = all_must.first() == Some(&compiled.query) || all_not.is_empty() && all_must.is_empty();
            prop_assert!(
                collapsed || (all_must.starts_with(&must) && all_not.starts_with(&not)),
                "{must:?} {not:?} not a prefix of {all_must:?} {all_not:?}"
            );
        }
    }
}

/// The required and excluded clauses of a compiled query, in text order; a root that is not a plain Bool is one.
fn clauses(q: &Query) -> (Vec<Query>, Vec<Query>) {
    match q {
        Query::MatchAll => (Vec::new(), Vec::new()),
        Query::Bool(b) if b.should.is_empty() && b.filter.is_empty() => {
            (b.must.clone(), b.must_not.clone())
        }
        other => (vec![other.clone()], Vec::new()),
    }
}

#[test]
fn fields_that_split_a_word_differently_do_not_constrain_each_other() {
    let schema = SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field(
            "sku",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("whitespace").unwrap(),
                ..TextOptions::default()
            }),
        )
        .build()
        .unwrap();
    let limits = QueryLimits::default();
    let compile = |text: &str| {
        let compiled = compiler(&schema).compile(text, &DefaultFields::all(&schema), &limits);
        compiled.query.validate(&schema, &limits).unwrap();
        show(&schema, &compiled.query)
    };
    // Same split: one combined per span.
    assert_eq!(compile("shoes"), "(sku:shoes^1|title:shoes^1)");
    // Different splits: each field requires its own tokens, one field must match.
    assert_eq!(
        compile("e-mail"),
        "{?(sku:e-mail^1) ?{+(title:e^1) +(title:mail^1)}}"
    );
    assert_eq!(
        compile("Red e-mail"),
        "{+(sku:Red^1|title:red^1) +{?(sku:e-mail^1) ?{+(title:e^1) +(title:mail^1)}}}"
    );
    // A field without tokens for the word does not count.
    assert_eq!(compile("!!"), "(sku:!!^1)");
}

#[test]
fn phrases_score_with_the_schema_boost() {
    let schema = schema();
    let fields = DefaultFields::parse(&schema, &["title^5"]).unwrap();
    let limits = QueryLimits::default();
    assert_eq!(
        compile_with("\"red shoes\" red", &fields, &limits).0,
        "{+title:\"red@0 shoes@1\" +(title:red^5)}"
    );
}

#[test]
fn one_long_word_compiles_quickly_and_is_cut() {
    // 4096 tokens per field: grouping is linear, and the clause is over max_terms.
    let schema = schema();
    let text = "a-".repeat(4096);
    let limits = QueryLimits::default();
    let started = std::time::Instant::now();
    let compiled = compiler(&schema).compile(&text, &DefaultFields::all(&schema), &limits);
    assert_eq!(
        (compiled.query, compiled.truncated),
        (Query::MatchAll, true)
    );
    // Generous for debug builds; the quadratic grouping took far longer.
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn filter_numbers_follow_the_document_forms() {
    let schema = schema();
    let f = |name| id(&schema, name);
    assert_eq!(
        filter(r#"{"term": ["at", -0.0]}"#),
        Ok(Filter::Term {
            field: f("at"),
            value: FilterValue::Datetime(0)
        })
    );
    for json in [r#"{"term": ["stock", 0.0]}"#, r#"{"term": ["stock", 0e0]}"#] {
        assert!(
            matches!(filter(json), Err(QueryError::FilterValue { .. })),
            "{json}"
        );
    }
    // The field's type is checked before the value's.
    for json in [
        r#"{"range": ["brand", {"gt": 1}]}"#,
        r#"{"term": ["title", 1]}"#,
        r#"{"terms": ["notes", [1]]}"#,
    ] {
        assert!(
            matches!(filter(json), Err(QueryError::WrongFieldType { .. })),
            "{json}"
        );
    }
}

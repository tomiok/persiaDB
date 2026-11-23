//! Hard and soft filters across the relaxation cascade (SPEC §7.6, §7.1.2; ROADMAP 8.2.13, ADR-0056).
//!
//! A request's `filter` is hard: no level drops it. L0 to L4 keep it at the root, L5 `fallback` keeps it, and the
//! filter step (L6, `filters: "last_resort"`) keeps it too. The request's `soft_filter` holds the filters L6 may drop,
//! and only L6: every earlier level applies them as filters. A filter given as both is hard. With `last_resort` and
//! no soft filter there is no L6 step. A programmatic query's root filters are hard unless the caller marks them soft
//! (`RelaxedQuery::with_soft_filter`); a compiled query's `filter` stays hard even if marked. A filter the root of a
//! query a caller put in (`Compiled::with_query`) holds itself is hard too, also one equal to a soft filter or marked
//! soft, and L5 keeps every filter L0 applies or builds no step.
//!
//! Invariants, checked over random corpora (puts, upserts, deletes and commits, so the documents are spread over
//! segments and the memtable), random user text (words of a small vocabulary with a synonym group, typos, prefixes,
//! a word no document holds, excluded `-words`) compiled by `QueryCompiler`, random programmatic root `bool`s, random
//! hard and soft filters (`term`, `terms`, `range`, `exists`, `and`, `or`, `not`), random plans (levels, `fallback`,
//! `fill`, `min_results`, `filters`), every page walked by offset and by `search_after` cursor:
//! 1. No hit of any level, on any page, breaks a hard filter. The filters are evaluated by an oracle over the model's
//!    values of each document ([`passes`]), never by the engine.
//! 2. Every hit at a level other than L6 passes the soft filters too (L0 to L5 keep them).
//! 3. L6 hits exist only with `last_resort` and at least one effective soft filter (a soft filter equal to a hard one
//!    is hard, a marked filter the root does not hold marks nothing). Every L6 hit fails at least one soft filter: a
//!    document passing all of them matches L0, which L6 excludes. `filters_dropped` is set exactly when a hit is L6's.
//! 4. Facets count only documents that pass the hard filters: on a page holding every match, the facet counts are
//!    the counts over the hits; on any page, no count exceeds the live documents holding the value that pass the hard
//!    filters. Without `fill`, every page counts the same facets and states the same relaxation as the full page.
//! 5. Pages by cursor concatenate to the full ranking; pages by offset are its slices.
//!
//! Coverage: every level must find hits in some case, L6 among them beside a hard filter, for compiled and for
//! programmatic queries, or the property passes vacuously ([`hard_filters_hold_at_every_level`]).
//!
//! Seeds: fixed (`RngSeed::Fixed`, [`SEED`]), printed on failure; `PROPTEST_RNG_SEED` and `PROPTEST_CASES` override.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    missing_docs
)] // Tests: inputs are generated; a broken premise panics.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::Bound;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, RelaxedHit, RelaxedQuery, RelaxedResults,
    SearchRequest, SegmentIds, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::query::{
    BoolQuery, Compiled, DefaultFields, Filter, FilterValue, Query, QueryCompiler, QueryLimits,
    TermQuery,
};
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    CollectionSchema, Fallback, FieldId, FieldKind, FilterRelaxation, KeywordOptions, Level,
    Levels, SchemaBuilder, TextOptions,
};
use persia_engine::search::{FacetRequest, Projection, Pruning};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError, TestRunner};

const SEED: u64 = 0x8213_0000_0001;

/// The words documents hold. `sneakers`/`trainers` and `hat`/`cap` are synonym groups (L1). No keyword value is one.
const WORDS: [&str; 9] = [
    "red", "shoes", "sneakers", "trainers", "blue", "running", "hat", "cap", "socks",
];
/// Words only queries hold: typos one edit from a word (`shoees`, `runing`, `sokcs`, a transposition) and two
/// (`snaekers`, 8 characters), prefixes of 2 and 3 characters (L3), and a word no document holds (L2).
const QUERY_ONLY: [&str; 9] = [
    "shoees", "runing", "sokcs", "snaekers", "sho", "run", "sne", "bl", "zebra",
];
const TENANTS: [&str; 3] = ["t1", "t2", "t3"];
const BRANDS: [&str; 3] = ["nike", "puma", "asics"];
const TAGS: [&str; 3] = ["sale", "new", "eco"];
/// Filter values: those documents hold, and one none does.
const TENANT_VALUES: [&str; 4] = ["t1", "t2", "t3", "t9"];
const BRAND_VALUES: [&str; 4] = ["nike", "puma", "asics", "reebok"];
const TAG_VALUES: [&str; 4] = ["sale", "new", "eco", "old"];
const NS: [i64; 4] = [-1, 0, 1, 2];
const IDS: u8 = 16;
/// Page sizes the walks use.
const LIMITS: [usize; 4] = [1, 2, 3, 5];
/// A page that holds every match.
const ALL: usize = 1000;

fn query_word(i: usize) -> &'static str {
    WORDS
        .get(i)
        .copied()
        .unwrap_or_else(|| QUERY_ONLY[i - WORDS.len()])
}

// ---------------------------------------------------------------------------------------------------------------------
// Schema, documents, database
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Fields {
    body: FieldId,
    tenant: FieldId,
    brand: FieldId,
    tags: FieldId,
    n: FieldId,
}

fn schema() -> CollectionSchema {
    let facet = || {
        FieldKind::Keyword(KeywordOptions {
            facet: true,
            ..KeywordOptions::default()
        })
    };
    SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tenant", facet())
        .field("brand", facet())
        .field("tags", FieldKind::Keyword(KeywordOptions::default()))
        .field("n", FieldKind::I64 { stored: false })
        .synonyms(vec![
            vec!["sneakers".into(), "trainers".into()],
            vec!["hat".into(), "cap".into()],
        ])
        .build()
        .unwrap()
}

fn fields(schema: &CollectionSchema) -> Fields {
    let id = |name: &str| schema.field(name).unwrap().id();
    Fields {
        body: id("body"),
        tenant: id("tenant"),
        brand: id("brand"),
        tags: id("tags"),
        n: id("n"),
    }
}

/// A document's values, as the model holds them: the oracle evaluates filters on these.
#[derive(Debug, Clone)]
struct Values {
    body: Vec<usize>,
    tenant: usize,
    brand: Option<usize>,
    tags: Vec<usize>,
    n: Option<i64>,
}

impl Values {
    fn document(&self, f: &Fields, id: &str) -> Document {
        let text: Vec<&str> = self.body.iter().map(|&w| WORDS[w]).collect();
        let mut doc = Document::new(id)
            .with(f.body, Value::String(text.join(" ")))
            .with(f.tenant, Value::String(TENANTS[self.tenant].into()));
        if let Some(b) = self.brand {
            doc = doc.with(f.brand, Value::String(BRANDS[b].into()));
        }
        if !self.tags.is_empty() {
            let tags = self.tags.iter().map(|&t| TAGS[t].to_owned()).collect();
            doc = doc.with(f.tags, Value::Strings(tags));
        }
        if let Some(n) = self.n {
            doc = doc.with(f.n, Value::I64(n));
        }
        doc
    }
}

fn database(schema: CollectionSchema) -> Database<SimFile> {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(13),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: usize::MAX,
                deleted_percent: 100,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [8; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    db
}

fn exec() -> Execution {
    Execution::new(Pruning::default())
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

// ---------------------------------------------------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------------------------------------------------

/// Whether `filter` selects a document of `v` (SPEC §7.1.2), evaluated on the model's values.
fn passes(filter: &Filter, v: &Values, f: &Fields) -> bool {
    match filter {
        Filter::Term { field, value } => holds(*field, value, v, f),
        Filter::Terms { field, values } => values.iter().any(|value| holds(*field, value, v, f)),
        Filter::Range {
            field,
            lower,
            upper,
        } => {
            assert_eq!(*field, f.n, "a range on another field");
            let above = |n: i64| match lower {
                Bound::Unbounded => true,
                Bound::Included(FilterValue::I64(b)) => n >= *b,
                Bound::Excluded(FilterValue::I64(b)) => n > *b,
                other => panic!("bound {other:?}"),
            };
            let below = |n: i64| match upper {
                Bound::Unbounded => true,
                Bound::Included(FilterValue::I64(b)) => n <= *b,
                Bound::Excluded(FilterValue::I64(b)) => n < *b,
                other => panic!("bound {other:?}"),
            };
            v.n.is_some_and(|n| above(n) && below(n))
        }
        Filter::Exists { field } => {
            if *field == f.brand {
                v.brand.is_some()
            } else if *field == f.tags {
                !v.tags.is_empty()
            } else if *field == f.n {
                v.n.is_some()
            } else {
                panic!("exists on {field:?}")
            }
        }
        Filter::And(all) => all.iter().all(|x| passes(x, v, f)),
        Filter::Or(any) => any.iter().any(|x| passes(x, v, f)),
        Filter::Not(x) => !passes(x, v, f),
        other => panic!("filter {other:?}"),
    }
}

fn holds(field: FieldId, value: &FilterValue, v: &Values, f: &Fields) -> bool {
    match value {
        FilterValue::Keyword(s) if field == f.tenant => TENANTS[v.tenant] == s,
        FilterValue::Keyword(s) if field == f.brand => v.brand.is_some_and(|b| BRANDS[b] == s),
        FilterValue::Keyword(s) if field == f.tags => v.tags.iter().any(|&t| TAGS[t] == s),
        FilterValue::I64(x) if field == f.n => v.n == Some(*x),
        _ => panic!("value {value:?} on {field:?}"),
    }
}

fn passes_all(filters: &[Filter], v: &Values, f: &Fields) -> bool {
    filters.iter().all(|x| passes(x, v, f))
}

// ---------------------------------------------------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Put(u8, Values),
    Delete(u8),
    Commit,
}

fn values() -> impl Strategy<Value = Values> {
    (
        prop::collection::vec(0..WORDS.len(), 1..6),
        0..TENANTS.len(),
        prop::option::weighted(0.8, 0..BRANDS.len()),
        prop::collection::vec(0..TAGS.len(), 0..3),
        prop::option::weighted(0.7, prop::sample::select(&NS[..])),
    )
        .prop_map(|(body, tenant, brand, tags, n)| Values {
            body,
            tenant,
            brand,
            tags,
            n,
        })
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (0..IDS, values()).prop_map(|(id, v)| Op::Put(id, v)),
        1 => (0..IDS).prop_map(Op::Delete),
        2 => Just(Op::Commit),
    ]
}

/// A range bound on `n`: none, inclusive or exclusive.
fn bound(kind: u8, v: i64) -> Bound<FilterValue> {
    match kind {
        0 => Bound::Unbounded,
        1 => Bound::Included(FilterValue::I64(v)),
        _ => Bound::Excluded(FilterValue::I64(v)),
    }
}

/// A tenancy filter: `tenant` is one value.
fn tenancy(f: Fields) -> impl Strategy<Value = Filter> {
    prop::sample::select(&TENANTS[..]).prop_map(move |t| Filter::Term {
        field: f.tenant,
        value: FilterValue::Keyword(t.into()),
    })
}

/// A brand filter: `brand` is one value.
fn brand(f: Fields) -> impl Strategy<Value = Filter> {
    prop::sample::select(&BRANDS[..]).prop_map(move |b| Filter::Term {
        field: f.brand,
        value: FilterValue::Keyword(b.into()),
    })
}

/// A filter of the kinds SPEC §7.1.2 allows on this schema, a tenancy-like `tenant` term the most common.
fn filter(f: Fields) -> impl Strategy<Value = Filter> {
    let keyword = |field: FieldId, values: &'static [&'static str]| {
        prop::sample::select(values).prop_map(move |s| Filter::Term {
            field,
            value: FilterValue::Keyword(s.into()),
        })
    };
    let keywords = |field: FieldId, values: &'static [&'static str]| {
        prop::collection::vec(prop::sample::select(values), 1..3).prop_map(move |vs| {
            Filter::Terms {
                field,
                values: vs
                    .into_iter()
                    .map(|s| FilterValue::Keyword(s.into()))
                    .collect(),
            }
        })
    };
    let n = f.n;
    let leaf = prop_oneof![
        5 => keyword(f.tenant, &TENANT_VALUES),
        1 => keywords(f.tenant, &TENANT_VALUES),
        4 => keyword(f.brand, &BRAND_VALUES),
        1 => keywords(f.brand, &BRAND_VALUES),
        2 => keyword(f.tags, &TAG_VALUES),
        1 => (-1_i64..4).prop_map(move |x| Filter::Term { field: n, value: FilterValue::I64(x) }),
        1 => prop::collection::vec(-1_i64..4, 1..3).prop_map(move |xs| Filter::Terms {
            field: n,
            values: xs.into_iter().map(FilterValue::I64).collect(),
        }),
        2 => (0..3_u8, 0..3_u8, prop::sample::select(&NS[..]), prop::sample::select(&NS[..])).prop_map(
            move |(lo, hi, a, b)| {
                let (a, b) = (a.min(b), a.max(b));
                // At least one bound, and not empty: equal bounds are both inclusive (SPEC §7.1.2).
                let (lo, hi) = match (lo, hi) {
                    (0, 0) => (1, 0),
                    (lo, hi) if a == b && lo != 0 && hi != 0 => (1, 1),
                    kinds => kinds,
                };
                Filter::Range { field: n, lower: bound(lo, a), upper: bound(hi, b) }
            },
        ),
        1 => Just(Filter::Exists { field: f.brand }),
        1 => Just(Filter::Exists { field: f.tags }),
        1 => Just(Filter::Exists { field: f.n }),
        // Selects nothing (SPEC §7.1.2): `terms` of no value, `or` of no filter.
        1 => Just(Filter::Terms { field: f.tenant, values: Vec::new() }),
        1 => Just(Filter::Or(Vec::new())),
    ];
    leaf.prop_recursive(2, 6, 2, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Filter::And),
            prop::collection::vec(inner.clone(), 1..3).prop_map(Filter::Or),
            inner.prop_map(|x| Filter::Not(Box::new(x))),
        ]
    })
}

/// A soft filter of user text: a filter of its own, or the hard filter at this index (modulo their count), which
/// makes it hard.
#[derive(Debug, Clone)]
enum Soft {
    Own(Filter),
    Hard(usize),
}

/// A filter of a caller's root: one of its own, or the soft filter at this index (modulo their count), which the
/// caller's root then holds itself, so it is hard (ROADMAP 8.2.13).
#[derive(Debug, Clone)]
enum CallerFilter {
    Own(Filter),
    Soft(usize),
}

/// A query a caller puts in place of the compiled one (`Compiled::with_query`): a root `bool` of `body` words, with
/// filters of its own.
#[derive(Debug, Clone)]
struct Caller {
    must: Vec<usize>,
    should: Vec<usize>,
    /// Its root's own filters, and whether the caller also marks each soft (`RelaxedQuery::with_soft_filter`):
    /// hard, whatever they equal, marked or not.
    filters: Vec<(CallerFilter, bool)>,
    /// Put in before the filters are joined, or after.
    before: bool,
}

#[derive(Debug, Clone)]
enum Form {
    /// User text compiled with `QueryCompiler`, then the request's `filter` and `soft_filter`.
    Text {
        /// Query words (indexes for [`query_word`]) and whether each is excluded (`-word`).
        words: Vec<(usize, bool)>,
        hard: Vec<Filter>,
        soft: Vec<Soft>,
        /// `with_soft_filter` before `with_filter`, or after.
        soft_first: bool,
        /// Which hard filters the caller also marks soft through `RelaxedQuery::with_soft_filter`: they stay hard.
        marks: Vec<bool>,
        caller: Option<Caller>,
    },
    /// A programmatic root `bool` of `body` terms, its filters marked soft or not.
    Query {
        must: Vec<usize>,
        should: Vec<usize>,
        must_not: Vec<usize>,
        minimum_should_match: u32,
        filters: Vec<(Filter, bool)>,
        /// A marked filter the root may not hold: it marks nothing then.
        stray: Option<Filter>,
    },
}

#[derive(Debug, Clone)]
struct Search {
    form: Form,
    last_resort: bool,
    fill: bool,
    min_results: u32,
    /// Which of the four word levels the plan keeps (`None`: `"default"`).
    levels: Option<Vec<bool>>,
    fallback: bool,
    limit: usize,
    facets: bool,
}

#[derive(Debug, Clone)]
struct Case {
    ops: Vec<Op>,
    searches: Vec<Search>,
}

/// An index for [`query_word`]: a word documents hold, two times in three.
fn word_index() -> impl Strategy<Value = usize> {
    prop_oneof![
        2 => 0..WORDS.len(),
        1 => WORDS.len()..WORDS.len() + QUERY_ONLY.len(),
    ]
}

fn text_form(f: Fields) -> impl Strategy<Value = Form> {
    (
        prop_oneof![
            3 => prop::collection::vec((word_index(), prop::bool::weighted(0.15)), 1..3),
            // A typo or a prefix alone, so L3 and L4 meet L0's misses often.
            1 => (WORDS.len()..WORDS.len() + QUERY_ONLY.len()).prop_map(|w| vec![(w, false)]),
        ],
        prop::collection::vec(prop_oneof![2 => tenancy(f), 3 => filter(f)], 0..3),
        prop::collection::vec(
            prop_oneof![
                2 => brand(f).prop_map(Soft::Own),
                3 => filter(f).prop_map(Soft::Own),
                1 => (0..3_usize).prop_map(Soft::Hard),
            ],
            0..3,
        ),
        any::<bool>(),
        prop::collection::vec(prop::bool::weighted(0.2), 3),
        prop::option::weighted(
            0.35,
            (
                prop::collection::vec(0..WORDS.len(), 0..3),
                prop::collection::vec(0..WORDS.len(), 0..3),
                prop::collection::vec(
                    (
                        prop_oneof![
                            1 => filter(f).prop_map(CallerFilter::Own),
                            2 => (0..3_usize).prop_map(CallerFilter::Soft),
                        ],
                        prop::bool::weighted(0.4),
                    ),
                    0..3,
                ),
                any::<bool>(),
            )
                .prop_map(|(must, should, filters, before)| Caller {
                    must,
                    should,
                    filters,
                    before,
                }),
        ),
    )
        .prop_map(
            |(words, hard, soft, soft_first, marks, caller)| Form::Text {
                words,
                hard,
                soft,
                soft_first,
                marks,
                caller,
            },
        )
}

fn query_form(f: Fields) -> impl Strategy<Value = Form> {
    (
        prop::collection::vec(word_index(), 0..4),
        prop::collection::vec(word_index(), 0..3),
        prop::collection::vec(0..WORDS.len(), 0..2),
        prop_oneof![3 => Just(0_u32), 1 => 1..3_u32],
        prop::collection::vec(
            (
                prop_oneof![2 => tenancy(f), 1 => brand(f), 3 => filter(f)],
                any::<bool>(),
            ),
            0..4,
        ),
        prop::option::weighted(0.15, filter(f)),
    )
        .prop_map(
            |(must, should, must_not, minimum_should_match, filters, stray)| Form::Query {
                must,
                should,
                must_not,
                minimum_should_match,
                filters,
                stray,
            },
        )
}

fn search(f: Fields) -> impl Strategy<Value = Search> {
    (
        prop_oneof![3 => text_form(f), 2 => query_form(f)],
        prop::bool::weighted(0.7),
        any::<bool>(),
        1..=u32::from(IDS),
        prop::option::weighted(0.4, prop::collection::vec(any::<bool>(), 4)),
        prop::bool::weighted(0.4),
        prop::sample::select(&LIMITS[..]),
        any::<bool>(),
    )
        .prop_map(
            |(form, last_resort, fill, min_results, levels, fallback, limit, facets)| Search {
                form,
                last_resort,
                fill,
                min_results,
                levels,
                fallback,
                limit,
                facets,
            },
        )
}

fn case(f: Fields) -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(op(), 1..40),
        prop::collection::vec(search(f), 1..4),
    )
        .prop_map(|(ops, searches)| Case { ops, searches })
}

// ---------------------------------------------------------------------------------------------------------------------
// Building a search
// ---------------------------------------------------------------------------------------------------------------------

/// A search's L0 and what the oracle holds it to.
struct Built {
    query: Owned,
    /// Never dropped by any level.
    hard: Vec<Filter>,
    /// Dropped by L6 alone: the soft filters that are not hard (a compiled query's `soft_filter`, a programmatic
    /// root's marked filters).
    soft: Vec<Filter>,
}

enum Owned {
    Compiled(Box<Compiled>, Vec<Filter>),
    Query(Query, Vec<Filter>),
}

impl Owned {
    fn relaxed(&self) -> RelaxedQuery<'_> {
        match self {
            Self::Compiled(compiled, marks) => {
                RelaxedQuery::from(compiled.as_ref()).with_soft_filter(marks)
            }
            Self::Query(query, marks) => RelaxedQuery::from(query).with_soft_filter(marks),
        }
    }
}

fn dedup(filters: impl IntoIterator<Item = Filter>) -> Vec<Filter> {
    let mut out: Vec<Filter> = Vec::new();
    for x in filters {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}

fn term(f: &Fields, word: &str) -> Query {
    Query::Term(TermQuery {
        field: f.body,
        term: word.into(),
    })
}

fn build(
    schema: &CollectionSchema,
    f: &Fields,
    form: &Form,
    cover: &mut dyn FnMut(&str),
) -> Result<Built, TestCaseError> {
    match form {
        Form::Text {
            words,
            hard,
            soft,
            soft_first,
            marks,
            caller,
        } => {
            let text: Vec<String> = words
                .iter()
                .map(|&(w, excluded)| {
                    let w = query_word(w);
                    if excluded { format!("-{w}") } else { w.into() }
                })
                .collect();
            let mut compiled = QueryCompiler::new(schema, &AnalyzerRegistry::builtin())
                .unwrap()
                .compile(
                    &text.join(" "),
                    &DefaultFields::all(schema),
                    &QueryLimits::default(),
                );
            let soft: Vec<Filter> = soft
                .iter()
                .filter_map(|s| match s {
                    Soft::Own(x) => Some(x.clone()),
                    Soft::Hard(i) => (!hard.is_empty()).then(|| hard[i % hard.len()].clone()),
                })
                .collect();
            if soft.iter().any(|s| hard.contains(s)) {
                cover("soft filter equal to a hard one");
            }
            // The caller's own root filters (ROADMAP 8.2.13): hard, a copy of a soft filter included.
            // With each, whether the caller also marks it soft.
            let own_marked: Vec<(Filter, bool)> = caller.as_ref().map_or_else(Vec::new, |c| {
                c.filters
                    .iter()
                    .filter_map(|(x, m)| match x {
                        CallerFilter::Own(x) => Some((x.clone(), *m)),
                        CallerFilter::Soft(i) => {
                            (!soft.is_empty()).then(|| (soft[i % soft.len()].clone(), *m))
                        }
                    })
                    .collect()
            });
            let own = dedup(own_marked.iter().map(|(x, _)| x.clone()));
            let caller = caller.as_ref().map(|c| {
                let query = Query::Bool(BoolQuery {
                    must: c.must.iter().map(|&w| term(f, WORDS[w])).collect(),
                    should: c.should.iter().map(|&w| term(f, WORDS[w])).collect(),
                    filter: own.clone(),
                    ..BoolQuery::default()
                });
                (query, c.before)
            });
            let wraps = caller
                .as_ref()
                .is_some_and(|(q, _)| matches!(q, Query::Bool(b) if !b.should.is_empty()));
            if let Some((query, true)) = &caller {
                compiled = compiled.with_query(query.clone());
            }
            compiled = if *soft_first {
                compiled
                    .with_soft_filter(soft.clone())
                    .with_filter(hard.clone())
            } else {
                compiled
                    .with_filter(hard.clone())
                    .with_soft_filter(soft.clone())
            };
            if let Some((query, false)) = caller {
                compiled = compiled.with_query(query);
            }
            let want_hard = dedup(hard.iter().cloned());
            let want_soft = dedup(soft.into_iter().filter(|s| !want_hard.contains(s)));
            // The compilation keeps each kind beside its query, in the order joined, a filter of both kinds hard.
            prop_assert_eq!(compiled.filter(), &want_hard[..]);
            prop_assert_eq!(compiled.soft_filter(), &want_soft[..]);
            // A hard filter marked soft by the caller stays hard, and so does a filter of the caller's own root: a
            // compiled query's soft filters are its request's `soft_filter` and nothing else (SPEC §7.6).
            let marked_hard: Vec<Filter> = want_hard
                .iter()
                .zip(marks.iter().cycle())
                .filter(|(_, m)| **m)
                .map(|(x, _)| x.clone())
                .collect();
            let marked_own: Vec<Filter> = own_marked
                .into_iter()
                .filter_map(|(x, m)| m.then_some(x))
                .collect();
            if !marked_hard.is_empty() {
                cover("hard filter marked soft");
            }
            if !marked_own.is_empty() {
                cover(if wraps {
                    "caller filter marked soft (wrapped)"
                } else {
                    "caller filter marked soft (in place)"
                });
            }
            let marked = dedup(marked_hard.into_iter().chain(marked_own));
            if want_soft.iter().any(|s| own.contains(s)) {
                cover(if wraps {
                    "caller filter equal to a soft one (wrapped)"
                } else {
                    "caller filter equal to a soft one (in place)"
                });
            }
            // The oracle's hard filters: the request's and the caller's own; its soft ones, the rest.
            let soft = want_soft
                .iter()
                .filter(|s| !own.contains(s))
                .cloned()
                .collect();
            Ok(Built {
                query: Owned::Compiled(Box::new(compiled), marked),
                hard: dedup(want_hard.into_iter().chain(own)),
                soft,
            })
        }
        Form::Query {
            must,
            should,
            must_not,
            minimum_should_match,
            filters,
            stray,
        } => {
            let words = |ws: &[usize]| ws.iter().map(|&w| term(f, query_word(w))).collect();
            let root: Vec<Filter> = filters.iter().map(|(x, _)| x.clone()).collect();
            let mut marks: Vec<Filter> = filters
                .iter()
                .filter(|(_, m)| *m)
                .map(|(x, _)| x.clone())
                .collect();
            if let Some(x) = stray {
                marks.push(x.clone());
            }
            let hard = dedup(root.iter().filter(|x| !marks.contains(x)).cloned());
            let soft = dedup(root.iter().filter(|x| marks.contains(x)).cloned());
            let query = Query::Bool(BoolQuery {
                must: words(must),
                should: words(should),
                must_not: must_not.iter().map(|&w| term(f, WORDS[w])).collect(),
                filter: root,
                minimum_should_match: *minimum_should_match,
            });
            Ok(Built {
                query: Owned::Query(query, marks),
                hard,
                soft,
            })
        }
    }
}

fn plan(schema: &CollectionSchema, s: &Search) -> RelaxPlan {
    let options = RelaxOptions {
        min_results: Some(s.min_results),
        fill: Some(s.fill),
        levels: s.levels.as_ref().map(|keep| {
            Levels::Custom(
                Level::ALL
                    .into_iter()
                    .zip(keep)
                    .filter_map(|(l, &k)| k.then_some(l))
                    .collect(),
            )
        }),
        fallback: Some(if s.fallback {
            Fallback::StaticRank
        } else {
            Fallback::None
        }),
        filters: Some(if s.last_resort {
            FilterRelaxation::LastResort
        } else {
            FilterRelaxation::Never
        }),
        ..RelaxOptions::default()
    };
    RelaxPlan::new(schema.relaxation(), &options).unwrap()
}

fn request(schema: &CollectionSchema, f: &Fields, limit: usize, facets: bool) -> SearchRequest {
    SearchRequest {
        limit,
        projection: Projection::None,
        facets: if facets {
            vec![
                FacetRequest::new(f.tenant, 1000, schema).unwrap(),
                FacetRequest::new(f.brand, 1000, schema).unwrap(),
            ]
        } else {
            Vec::new()
        },
        ..SearchRequest::default()
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------------------------------------------------

/// What a case's checks share.
struct Ctx<'a> {
    f: &'a Fields,
    live: &'a BTreeMap<String, Values>,
    built: &'a Built,
    search: &'a Search,
}

impl Ctx<'_> {
    fn doc(&self, hit: &RelaxedHit) -> Result<&Values, TestCaseError> {
        let id = hit.hit.document.id();
        self.live
            .get(id)
            .ok_or_else(|| TestCaseError::fail(format!("{id} is not a live document")))
    }

    /// Invariants 1 to 3 on each hit of a page.
    fn check_hits(&self, hits: &[RelaxedHit], what: &str) -> Result<(), TestCaseError> {
        let Built { hard, soft, .. } = self.built;
        for hit in hits {
            let id = hit.hit.document.id();
            let v = self.doc(hit)?;
            for x in hard {
                prop_assert!(
                    passes(x, v, self.f),
                    "{what}: {id} ({v:?}) at {:?} breaks the hard filter {x:?}",
                    hit.level
                );
            }
            if hit.level == RelaxLevel::Filters {
                prop_assert!(
                    self.search.last_resort && !soft.is_empty(),
                    "{what}: {id} is an L6 hit without a soft filter to drop (last_resort: {})",
                    self.search.last_resort
                );
                prop_assert!(
                    !passes_all(soft, v, self.f),
                    "{what}: L6 hit {id} passes every soft filter, so L0 matched it"
                );
            } else {
                for x in soft {
                    prop_assert!(
                        passes(x, v, self.f),
                        "{what}: {id} ({v:?}) at {:?} breaks the soft filter {x:?}, which only L6 drops",
                        hit.level
                    );
                }
            }
        }
        Ok(())
    }

    /// The facets of a page: one map of `(facet index, value) -> count`.
    fn facets(out: &RelaxedResults) -> BTreeMap<(usize, Vec<u8>), u64> {
        out.facets
            .iter()
            .enumerate()
            .flat_map(|(i, counts)| {
                counts
                    .values
                    .iter()
                    .map(move |v| ((i, v.value.clone()), v.count))
            })
            .collect()
    }

    /// Invariant 4's bound: no count above the live documents holding the value that pass the hard filters.
    fn check_facet_bound(&self, out: &RelaxedResults, what: &str) -> Result<(), TestCaseError> {
        if !self.search.facets {
            prop_assert!(out.facets.is_empty());
            return Ok(());
        }
        prop_assert_eq!(out.facets.len(), 2);
        let mut most: BTreeMap<(usize, Vec<u8>), u64> = BTreeMap::new();
        for v in self.live.values() {
            if passes_all(&self.built.hard, v, self.f) {
                *most
                    .entry((0, TENANTS[v.tenant].as_bytes().to_vec()))
                    .or_default() += 1;
                if let Some(b) = v.brand {
                    *most.entry((1, BRANDS[b].as_bytes().to_vec())).or_default() += 1;
                }
            }
        }
        for ((i, value), count) in Self::facets(out) {
            let bound = most.get(&(i, value.clone())).copied().unwrap_or(0);
            prop_assert!(
                count <= bound,
                "{what}: facet {i} counts {count} for {:?}, but only {bound} live documents holding it pass the hard \
                 filters",
                String::from_utf8_lossy(&value)
            );
        }
        Ok(())
    }

    /// Invariant 4 on the page that holds every match: the facets count exactly its hits.
    fn check_facets_exact(&self, out: &RelaxedResults) -> Result<(), TestCaseError> {
        if !self.search.facets {
            return Ok(());
        }
        let mut want: BTreeMap<(usize, Vec<u8>), u64> = BTreeMap::new();
        for hit in &out.hits {
            let v = self.doc(hit)?;
            *want
                .entry((0, TENANTS[v.tenant].as_bytes().to_vec()))
                .or_default() += 1;
            if let Some(b) = v.brand {
                *want.entry((1, BRANDS[b].as_bytes().to_vec())).or_default() += 1;
            }
        }
        prop_assert_eq!(Self::facets(out), want, "facets of the full page");
        Ok(())
    }
}

fn strip(hits: &[RelaxedHit]) -> Vec<(String, usize, u64)> {
    hits.iter()
        .map(|h| {
            (
                h.hit.document.id().to_owned(),
                h.step,
                h.hit.score.to_bits(),
            )
        })
        .collect()
}

type Coverage = RefCell<BTreeMap<String, usize>>;

fn bump(coverage: &Coverage, key: &str) {
    *coverage.borrow_mut().entry(key.to_owned()).or_default() += 1;
}

fn run_case(case: &Case, coverage: &Coverage) -> Result<(), TestCaseError> {
    let schema = schema();
    let f = fields(&schema);
    let mut db = database(schema);
    let mut live: BTreeMap<String, Values> = BTreeMap::new();
    for op in &case.ops {
        match op {
            Op::Put(id, v) => {
                let id = format!("d{id}");
                db.put("c", &v.document(&f, &id), Durability::Buffered)
                    .unwrap();
                live.insert(id, v.clone());
            }
            Op::Delete(id) => {
                let id = format!("d{id}");
                db.delete("c", &id, Durability::Buffered).unwrap();
                live.remove(&id);
            }
            Op::Commit => {
                db.commit().unwrap();
            }
        }
    }
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let schema = c.schema();
    for s in &case.searches {
        let form = match s.form {
            Form::Text { .. } => "text",
            Form::Query { .. } => "query",
        };
        let built = build(schema, &f, &s.form, &mut |key: &str| bump(coverage, key))?;
        let ctx = Ctx {
            f: &f,
            live: &live,
            built: &built,
            search: s,
        };
        let query = built.query.relaxed();
        let relax = plan(schema, s);
        let search = |request: &SearchRequest| {
            views
                .search_relaxed(query, request, &relax, &exec())
                .map_err(|e| TestCaseError::fail(format!("search failed: {e}")))
        };

        // The page that holds every match.
        let full = search(&request(schema, &f, ALL, s.facets))?;
        ctx.check_hits(&full.hits, "full page")?;
        ctx.check_facet_bound(&full, "full page")?;
        ctx.check_facets_exact(&full)?;
        let mut ids: Vec<&str> = full.hits.iter().map(|h| h.hit.document.id()).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        prop_assert_eq!(ids.len(), n, "a document twice");
        let l6 = full.hits.iter().any(|h| h.level == RelaxLevel::Filters);
        prop_assert_eq!(full.relaxation.filters_dropped, l6);
        if !s.last_resort || built.soft.is_empty() {
            prop_assert!(!full.relaxation.filters_dropped);
            prop_assert!(
                !full.relaxation.applied.iter().any(|a| a == "filters"),
                "a filter step without a soft filter: {:?}",
                full.relaxation.applied
            );
            prop_assert_ne!(full.relaxation.level, RelaxLevel::Filters);
        }
        if s.last_resort && built.soft.is_empty() {
            bump(
                coverage,
                &format!("last_resort without a soft filter ({form})"),
            );
        }
        for hit in &full.hits {
            bump(coverage, &format!("{} hit ({form})", hit.level.name()));
        }
        if l6 && !built.hard.is_empty() {
            bump(
                coverage,
                &format!("filters hit beside a hard filter ({form})"),
            );
            if s.facets {
                bump(coverage, "facets beside filters hits");
            }
        }

        // Pages by cursor: each checked, together the full ranking.
        let mut request = request(schema, &f, s.limit, s.facets);
        let mut walked = Vec::new();
        loop {
            let page = search(&request)?;
            ctx.check_hits(&page.hits, "cursor page")?;
            ctx.check_facet_bound(&page, "cursor page")?;
            if !s.fill {
                prop_assert_eq!(Ctx::facets(&page), Ctx::facets(&full), "cursor page facets");
                prop_assert_eq!(&page.relaxation, &full.relaxation, "cursor page relaxation");
            }
            let Some(last) = page.hits.last() else {
                break;
            };
            request.search_after = Some(last.cursor());
            walked.extend(page.hits);
            prop_assert!(walked.len() <= full.hits.len(), "paging does not end");
        }
        prop_assert_eq!(strip(&walked), strip(&full.hits), "pages by cursor");

        // Pages by offset: each checked, each a slice of the full ranking.
        let mut request = request.clone();
        request.search_after = None;
        let mut offset = 0;
        loop {
            request.offset = offset;
            let page = search(&request)?;
            ctx.check_hits(&page.hits, "offset page")?;
            ctx.check_facet_bound(&page, "offset page")?;
            if !s.fill {
                prop_assert_eq!(Ctx::facets(&page), Ctx::facets(&full), "offset page facets");
                prop_assert_eq!(&page.relaxation, &full.relaxation, "offset page relaxation");
            }
            let end = full.hits.len().min(offset + s.limit);
            let want = full.hits.get(offset..end).unwrap_or(&[]);
            prop_assert_eq!(strip(&page.hits), strip(want), "offset page at {}", offset);
            if page.hits.len() < s.limit {
                break;
            }
            offset += s.limit;
        }
    }
    Ok(())
}

fn run<S>(
    name: &'static str,
    cases: u32,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) where
    S: Strategy,
    S::Value: Debug,
{
    let seed = std::env::var("PROPTEST_RNG_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SEED);
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(cases);
    let config = ProptestConfig {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        source_file: Some(file!()),
        test_name: Some(name),
        ..ProptestConfig::default()
    };
    let mut runner = TestRunner::new(config);
    if let Err(e) = runner.run(&strategy, test) {
        panic!("{name} failed with PROPTEST_RNG_SEED={seed}: {e}\n{runner}");
    }
}

/// Invariants 1 to 5 over random corpora, queries, filters and plans. Every level must find hits in some case, L6
/// beside a hard filter for both kinds of query, or the property passes vacuously.
#[test]
fn hard_filters_hold_at_every_level() {
    let cases = if cfg!(feature = "slow") { 1024 } else { 128 };
    let f = fields(&schema());
    let coverage = Coverage::default();
    run("hard_filters_hold_at_every_level", cases, case(f), |case| {
        run_case(&case, &coverage)
    });
    let coverage = coverage.into_inner();
    eprintln!("coverage: {coverage:#?}");
    if std::env::var("PROPTEST_CASES").is_err() {
        for key in [
            "exact hit (text)",
            "synonyms hit (text)",
            "drop_terms hit (text)",
            "prefix hit (text)",
            "fuzzy hit (text)",
            "fallback hit (text)",
            "filters hit (text)",
            "exact hit (query)",
            "drop_terms hit (query)",
            "fallback hit (query)",
            "filters hit (query)",
            "filters hit beside a hard filter (text)",
            "filters hit beside a hard filter (query)",
            "facets beside filters hits",
            "last_resort without a soft filter (text)",
            "last_resort without a soft filter (query)",
            "soft filter equal to a hard one",
            "hard filter marked soft",
            "caller filter marked soft (in place)",
            "caller filter marked soft (wrapped)",
            "caller filter equal to a soft one (in place)",
            "caller filter equal to a soft one (wrapped)",
        ] {
            assert!(coverage.get(key).is_some_and(|&n| n > 0), "never: {key}");
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// By hand
// ---------------------------------------------------------------------------------------------------------------------

/// `(id, body, tenant, brand)`, over a segment and the memtable.
const SHOP: [(&str, &str, &str, &str); 6] = [
    ("a", "red shoes", "t1", "nike"),
    ("b", "red shoes", "t1", "puma"),
    ("c", "red shoes", "t2", "nike"),
    ("d", "red shoes", "t2", "puma"),
    ("e", "blue hat", "t1", "nike"),
    ("f", "red shoes", "t1", "asics"),
];

fn shop() -> Database<SimFile> {
    let schema = schema();
    let f = fields(&schema);
    let mut db = database(schema);
    for (i, (id, body, tenant, brand)) in SHOP.into_iter().enumerate() {
        let doc = Document::new(id)
            .with(f.body, Value::String(body.into()))
            .with(f.tenant, Value::String(tenant.into()))
            .with(f.brand, Value::String(brand.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i == 2 {
            db.commit().unwrap();
        }
    }
    db
}

fn keyword(field: FieldId, value: &str) -> Filter {
    Filter::Term {
        field,
        value: FilterValue::Keyword(value.into()),
    }
}

fn shop_plan(schema: &CollectionSchema, json: &str) -> RelaxPlan {
    RelaxPlan::new(schema.relaxation(), &RelaxOptions::from_json(json).unwrap()).unwrap()
}

fn levels(out: &RelaxedResults) -> Vec<(&str, RelaxLevel)> {
    let mut hits: Vec<(&str, RelaxLevel)> = out
        .hits
        .iter()
        .map(|h| (h.hit.document.id(), h.level))
        .collect();
    hits.sort_unstable_by_key(|h| h.0);
    hits
}

/// A tenancy filter is hard, a brand filter soft: under `last_resort`, L6 finds the tenant's documents of another
/// brand, never another tenant's, and L5 keeps both filters.
#[test]
fn last_resort_drops_the_soft_brand_and_keeps_the_hard_tenant() {
    let db = shop();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let schema = c.schema();
    let f = fields(schema);
    let compiled = QueryCompiler::new(schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .compile(
            "red shoes",
            &DefaultFields::all(schema),
            &QueryLimits::default(),
        )
        .with_filter([keyword(f.tenant, "t1")])
        .with_soft_filter([keyword(f.brand, "nike")]);
    let page = request(schema, &f, 10, true);
    let last = views
        .search_relaxed(
            &compiled,
            &page,
            &shop_plan(schema, r#"{"min_results": 6, "filters": "last_resort"}"#),
            &exec(),
        )
        .unwrap();
    // L0: `a` (t1, nike). L5: `e` (t1, nike, no `red shoes`). L6: `b` and `f`, t1's `red shoes` of other brands. Never
    // `c` or `d` (t2).
    assert_eq!(
        levels(&last),
        [
            ("a", RelaxLevel::Exact),
            ("b", RelaxLevel::Filters),
            ("e", RelaxLevel::Fallback),
            ("f", RelaxLevel::Filters),
        ]
    );
    assert!(last.relaxation.filters_dropped);
    assert_eq!(last.relaxation.level, RelaxLevel::Filters);
    // Facets count t1 alone: `a`, `b`, `e`, `f`.
    let tenants: Vec<(&[u8], u64)> = last.facets[0]
        .values
        .iter()
        .map(|v| (v.value.as_slice(), v.count))
        .collect();
    assert_eq!(tenants, [(&b"t1"[..], 4)]);

    // `"never"`: the soft filter holds at every level, L5 included.
    let never = views
        .search_relaxed(
            &compiled,
            &page,
            &shop_plan(schema, r#"{"min_results": 6}"#),
            &exec(),
        )
        .unwrap();
    assert_eq!(
        levels(&never),
        [("a", RelaxLevel::Exact), ("e", RelaxLevel::Fallback)]
    );
    assert!(!never.relaxation.filters_dropped);
}

/// `last_resort` with only a hard filter, or with a soft filter equal to the hard one, builds no filter step: the
/// response is `"never"`'s.
#[test]
fn last_resort_with_only_hard_filters_builds_no_filter_step() {
    let db = shop();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let schema = c.schema();
    let f = fields(schema);
    let compiler = QueryCompiler::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    let compile = || {
        compiler.compile(
            "red shoes",
            &DefaultFields::all(schema),
            &QueryLimits::default(),
        )
    };
    let tenant = keyword(f.tenant, "t1");
    let hard_only = compile().with_filter([tenant.clone()]);
    // Soft first, then the same filter hard: it is hard. Hard first, then soft: the soft one is not added.
    let both = [
        compile()
            .with_soft_filter([tenant.clone()])
            .with_filter([tenant.clone()]),
        compile()
            .with_filter([tenant.clone()])
            .with_soft_filter([tenant.clone()]),
    ];
    let page = request(schema, &f, 10, false);
    let last = shop_plan(schema, r#"{"min_results": 6, "filters": "last_resort"}"#);
    let never = shop_plan(schema, r#"{"min_results": 6}"#);
    let want = views
        .search_relaxed(&hard_only, &page, &never, &exec())
        .unwrap();
    for compiled in [&hard_only, &both[0], &both[1]] {
        assert_eq!(compiled.filter(), std::slice::from_ref(&tenant));
        assert_eq!(compiled.soft_filter(), &[] as &[Filter]);
        // Marking the hard filter soft changes nothing either.
        let marked = [tenant.clone()];
        for query in [
            RelaxedQuery::from(compiled),
            RelaxedQuery::from(compiled).with_soft_filter(&marked),
        ] {
            let out = views.search_relaxed(query, &page, &last, &exec()).unwrap();
            assert_eq!(strip(&out.hits), strip(&want.hits));
            assert_eq!(out.relaxation, want.relaxation);
            assert!(!out.relaxation.applied.iter().any(|a| a == "filters"));
            assert!(!out.relaxation.filters_dropped);
            // `a`, `b` and `f` by their words, `e` by L5: t1 alone.
            assert_eq!(
                levels(&out),
                [
                    ("a", RelaxLevel::Exact),
                    ("b", RelaxLevel::Exact),
                    ("e", RelaxLevel::Fallback),
                    ("f", RelaxLevel::Exact),
                ]
            );
        }
    }
}

/// A programmatic root's filters are hard unless marked: unmarked, `last_resort` builds no filter step; marking the
/// brand drops it alone; marking a filter the root does not hold marks nothing.
#[test]
fn a_programmatic_root_drops_only_the_filters_marked_soft() {
    let db = shop();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let schema = c.schema();
    let f = fields(schema);
    let (tenant, nike) = (keyword(f.tenant, "t1"), keyword(f.brand, "nike"));
    let query = Query::Bool(BoolQuery {
        must: vec![term(&f, "red"), term(&f, "shoes")],
        filter: vec![tenant.clone(), nike.clone()],
        ..BoolQuery::default()
    });
    let page = request(schema, &f, 10, false);
    let last = shop_plan(
        schema,
        r#"{"min_results": 6, "filters": "last_resort", "fallback": "none"}"#,
    );
    let unmarked = views.search_relaxed(&query, &page, &last, &exec()).unwrap();
    assert_eq!(levels(&unmarked), [("a", RelaxLevel::Exact)]);
    assert!(!unmarked.relaxation.filters_dropped);
    assert!(!unmarked.relaxation.applied.iter().any(|a| a == "filters"));
    let stray = [keyword(f.brand, "puma")];
    let out = views
        .search_relaxed(
            RelaxedQuery::from(&query).with_soft_filter(&stray),
            &page,
            &last,
            &exec(),
        )
        .unwrap();
    assert_eq!(strip(&out.hits), strip(&unmarked.hits));
    assert_eq!(out.relaxation, unmarked.relaxation);

    let soft = [nike];
    let marked = views
        .search_relaxed(
            RelaxedQuery::from(&query).with_soft_filter(&soft),
            &page,
            &last,
            &exec(),
        )
        .unwrap();
    assert_eq!(
        levels(&marked),
        [
            ("a", RelaxLevel::Exact),
            ("b", RelaxLevel::Filters),
            ("f", RelaxLevel::Filters),
        ]
    );
    assert!(marked.relaxation.filters_dropped);
}

/// ROADMAP 8.2.13 (batch review): a filter the root of a query a caller put in place of a compiled one holds itself is
/// the caller's, and hard: the filter step keeps it even when it equals the request's soft filter (the join does not
/// add it twice), in place or in the root the join wrapped, the query put in before the filters joined or after. A
/// caller filter of another value stays beside it, and the soft filter alone is dropped.
#[test]
fn a_callers_own_root_filter_stays_even_when_it_equals_a_soft_one() {
    let db = shop();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let schema = c.schema();
    let f = fields(schema);
    let qc = QueryCompiler::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    let (tenant, nike, asics) = (
        keyword(f.tenant, "t1"),
        keyword(f.brand, "nike"),
        keyword(f.brand, "asics"),
    );
    let plan = shop_plan(
        schema,
        r#"{"min_results": 6, "filters": "last_resort", "fallback": "none"}"#,
    );
    let page = request(schema, &f, 10, false);
    // In place: a root `bool` without `should` clauses. Wrapped: one with them, which the join wraps.
    let in_place = |filter: Vec<Filter>| {
        Query::Bool(BoolQuery {
            must: vec![term(&f, "red"), term(&f, "shoes")],
            filter,
            ..BoolQuery::default()
        })
    };
    let wrapped = |filter: Vec<Filter>| {
        Query::Bool(BoolQuery {
            must: vec![term(&f, "red")],
            should: vec![term(&f, "shoes")],
            filter,
            ..BoolQuery::default()
        })
    };
    let build = |caller: &Query, before: bool| {
        let compiled = qc.compile(
            "red shoes",
            &DefaultFields::all(schema),
            &QueryLimits::default(),
        );
        if before {
            compiled
                .with_query(caller.clone())
                .with_filter([tenant.clone()])
                .with_soft_filter([nike.clone()])
        } else {
            compiled
                .with_filter([tenant.clone()])
                .with_soft_filter([nike.clone()])
                .with_query(caller.clone())
        }
    };
    for caller in [in_place(vec![nike.clone()]), wrapped(vec![nike.clone()])] {
        for before in [false, true] {
            let compiled = build(&caller, before);
            assert_eq!(compiled.soft_filter(), std::slice::from_ref(&nike));
            let out = views
                .search_relaxed(&compiled, &page, &plan, &exec())
                .unwrap();
            // t1's nike `red shoes`, never `b` or `f` (t1, other brands): nothing is left to drop, so no filter step.
            assert_eq!(
                levels(&out),
                [("a", RelaxLevel::Exact)],
                "{caller:?} {before}"
            );
            assert!(!out.relaxation.filters_dropped);
            assert!(!out.relaxation.applied.iter().any(|a| a == "filters"));
        }
    }
    // The caller's own `asics` beside the soft `nike`: L0 finds none of both; the filter step drops `nike` alone and
    // finds t1's asics `red shoes`, `f`.
    for caller in [in_place(vec![asics.clone()]), wrapped(vec![asics.clone()])] {
        for before in [false, true] {
            let compiled = build(&caller, before);
            let out = views
                .search_relaxed(&compiled, &page, &plan, &exec())
                .unwrap();
            assert_eq!(
                levels(&out),
                [("f", RelaxLevel::Filters)],
                "{caller:?} {before}"
            );
            assert!(out.relaxation.filters_dropped);
        }
    }
}

/// ROADMAP 8.2.13 (re-review 2): a compiled query's soft filters are its request's `soft_filter` and nothing else
/// (SPEC §7.6). A filter of the caller's own root that the caller also marks soft through
/// `RelaxedQuery::with_soft_filter` stays hard, in place as where the join wrapped it: without a request soft filter
/// there is no filter step, and with one the step drops it alone, so no hit breaks the caller's filter.
#[test]
fn a_callers_own_root_filter_stays_hard_when_marked_soft() {
    let db = shop();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let schema = c.schema();
    let f = fields(schema);
    let qc = QueryCompiler::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    let (tenant, nike, asics) = (
        keyword(f.tenant, "t1"),
        keyword(f.brand, "nike"),
        keyword(f.brand, "asics"),
    );
    let plan = shop_plan(
        schema,
        r#"{"min_results": 6, "filters": "last_resort", "fallback": "none"}"#,
    );
    let page = request(schema, &f, 10, false);
    let in_place = Query::Bool(BoolQuery {
        must: vec![term(&f, "red"), term(&f, "shoes")],
        filter: vec![asics.clone()],
        ..BoolQuery::default()
    });
    let wrapped = Query::Bool(BoolQuery {
        must: vec![term(&f, "red")],
        should: vec![term(&f, "shoes")],
        filter: vec![asics.clone()],
        ..BoolQuery::default()
    });
    let marks = [asics.clone(), tenant.clone()];
    for caller in [&in_place, &wrapped] {
        for before in [false, true] {
            for soft in [None, Some(&nike)] {
                let mut compiled = qc.compile(
                    "red shoes",
                    &DefaultFields::all(schema),
                    &QueryLimits::default(),
                );
                if before {
                    compiled = compiled.with_query(caller.clone());
                }
                compiled = compiled
                    .with_filter([tenant.clone()])
                    .with_soft_filter(soft.cloned());
                if !before {
                    compiled = compiled.with_query(caller.clone());
                }
                let what = format!("{caller:?} {before} {soft:?}");
                let out = views
                    .search_relaxed(
                        RelaxedQuery::from(&compiled).with_soft_filter(&marks),
                        &page,
                        &plan,
                        &exec(),
                    )
                    .unwrap();
                // t1's asics `red shoes` alone, never `a` or `b` (t1, other brands): by its words without a soft
                // filter, which leaves nothing to drop; by the filter step dropping `nike` alone with one.
                let (level, dropped) = match soft {
                    None => (RelaxLevel::Exact, false),
                    Some(_) => (RelaxLevel::Filters, true),
                };
                assert_eq!(levels(&out), [("f", level)], "{what}");
                assert_eq!(out.relaxation.filters_dropped, dropped, "{what}");
                assert_eq!(
                    out.relaxation.applied.iter().any(|a| a == "filters"),
                    dropped,
                    "{what}"
                );
            }
        }
    }
}

/// ROADMAP 8.2.13 (batch review): L5 keeps the `must_not` and `filter` of a caller's root the join wrapped, and builds
/// no step when L0 holds a filter in a clause it drops (a programmatic `boost` of a filtered `bool`, or a filtered
/// `bool` a caller put in as its query's `must`): `match_all` would not apply that filter.
#[test]
fn fallback_never_selects_a_document_l0s_filters_reject() {
    let db = shop();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let schema = c.schema();
    let f = fields(schema);
    let compile = || {
        QueryCompiler::new(schema, &AnalyzerRegistry::builtin())
            .unwrap()
            .compile(
                "red shoes",
                &DefaultFields::all(schema),
                &QueryLimits::default(),
            )
    };
    let (tenant, nike, puma) = (
        keyword(f.tenant, "t1"),
        keyword(f.brand, "nike"),
        keyword(f.brand, "puma"),
    );
    let plan = shop_plan(schema, r#"{"min_results": 6}"#);
    let page = request(schema, &f, 10, false);
    let brand = |out: &RelaxedResults| -> Vec<String> {
        let mut ids: Vec<String> = out
            .hits
            .iter()
            .map(|h| h.hit.document.id().to_owned())
            .collect();
        ids.sort_unstable();
        ids
    };

    // A wrapped caller root: `red`, `shoes` optional, `puma`, not `hat`. L0 finds `b`; L5 selects t1's puma documents
    // without `hat`, `b` alone, already L0's. Before, L5 kept t1 alone and found `a`, `e` and `f`.
    let caller = Query::Bool(BoolQuery {
        must: vec![term(&f, "red")],
        should: vec![term(&f, "shoes")],
        must_not: vec![term(&f, "hat")],
        filter: vec![puma],
        ..BoolQuery::default()
    });
    let compiled = compile().with_filter([tenant.clone()]).with_query(caller);
    let out = views
        .search_relaxed(&compiled, &page, &plan, &exec())
        .unwrap();
    assert_eq!(levels(&out), [("b", RelaxLevel::Exact)]);
    assert!(out.relaxation.applied.iter().any(|a| a == "fallback"));

    // Programmatic: a `boost` of a `bool` filtered to t1. L5 builds no step; every hit is t1's `red shoes`.
    let filtered = |filter: Filter| {
        Query::Bool(BoolQuery {
            must: vec![term(&f, "red"), term(&f, "shoes")],
            filter: vec![filter],
            ..BoolQuery::default()
        })
    };
    let boosted = Query::Boost(persia_engine::query::BoostQuery {
        query: Box::new(filtered(tenant.clone())),
        boost: 2.0,
    });
    let out = views
        .search_relaxed(&boosted, &page, &plan, &exec())
        .unwrap();
    assert_eq!(brand(&out), ["a", "b", "f"]);
    assert!(out.hits.iter().all(|h| h.level != RelaxLevel::Fallback));
    assert!(!out.relaxation.applied.iter().any(|a| a == "fallback"));
    assert_eq!(out.relaxation.fallback, None);

    // Programmatic, a filtered `bool` as a root `must` clause: L2 does not drop it, L5 builds no step.
    let nested = Query::Bool(BoolQuery {
        must: vec![filtered(tenant.clone()), term(&f, "red")],
        ..BoolQuery::default()
    });
    let out = views
        .search_relaxed(&nested, &page, &plan, &exec())
        .unwrap();
    assert_eq!(brand(&out), ["a", "b", "f"]);
    assert!(
        !out.relaxation
            .applied
            .iter()
            .any(|a| a.starts_with("drop_terms") || a == "fallback")
    );

    // Compiled, a caller's query whose one `must` is filtered to nike, beside the hard t1: `a` alone, no L5 step.
    let compiled = compile()
        .with_filter([tenant])
        .with_query(Query::Bool(BoolQuery {
            must: vec![filtered(nike)],
            ..BoolQuery::default()
        }));
    let out = views
        .search_relaxed(&compiled, &page, &plan, &exec())
        .unwrap();
    assert_eq!(levels(&out), [("a", RelaxLevel::Exact)]);
    assert!(!out.relaxation.applied.iter().any(|a| a == "fallback"));
}

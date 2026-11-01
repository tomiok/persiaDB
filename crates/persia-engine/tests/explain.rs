//! Per-hit score breakdowns (ROADMAP 7.7.1; SPEC §7.7, §7.2).
//!
//! Invariants:
//! 1. An explanation's score is the hit's, bit for bit, in segments and the memtable, under every pruning strategy.
//! 2. A word lists each field holding it with its frequency, norm byte, decoded length, factor
//!    `w_f / (1 − b_f + b_f · len/avglen_f)` and contribution `tf · factor`, which sum to its `tf~`; `idf` and the
//!    score are SPEC §7.2's (checked by hand here, against the reference in `tests/differential.rs`).
//! 3. A `bool` lists every `must` and only the `should` clauses that match; a phrase its frequency; a `boost` its
//!    factor (a `fuzzy` term its edit-distance weight); a `constant_score` its score.
//! 4. A document the plan does not match (excluded, filtered out, deleted, or hidden by the memtable) has none, and
//!    a request without `explain` returns none.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::expect_used,
    clippy::many_single_char_names,
    missing_docs
)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_engine::db::{
    CollectionSnapshot, Database, DatabaseOptions, Located, ManualClock, SearchRequest, SegmentIds,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::MemtableView;
use persia_engine::query::{
    BoolQuery, BoostQuery, CombinedQuery, ConstantScoreQuery, Filter, FilterValue, FuzzyQuery,
    PhraseQuery, PhraseTerm, PrefixQuery, Query, TermQuery, WeightedTerm,
};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::{Explanation, Pruning, SearchPlan, WordExplanation};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

const STRATEGIES: [Pruning; 3] = [
    Pruning::Exhaustive,
    Pruning::MaxScore,
    Pruning::BlockMaxWand,
];

fn schema() -> CollectionSchema {
    let text = |boost| {
        FieldKind::Text(TextOptions {
            boost,
            positions: true,
            ..TextOptions::default()
        })
    };
    SchemaBuilder::new("c")
        .field("title", text(2.0))
        .field("body", text(1.0))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

struct Fixture {
    db: Database<SimFile>,
    title: FieldId,
    body: FieldId,
    tag: FieldId,
}

/// `a`, `b` and `gone` in a segment (`gone` then deleted), `c` in the memtable, and `b` replaced in the memtable by
/// a version whose title holds `blue`.
fn fixture() -> Fixture {
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [5; 16], options).unwrap();
    let schema = schema();
    let id = |name| schema.field(name).unwrap().id();
    let (title, body, tag) = (id("title"), id("body"), id("tag"));
    db.create_collection(schema).unwrap();
    let doc = |id: &str, t: &str, b: &str, g: &str| {
        Document::new(id)
            .with(title, Value::String(t.into()))
            .with(body, Value::String(b.into()))
            .with(tag, Value::String(g.into()))
    };
    for d in [
        doc("a", "red shoes", "red red running shoes", "x"),
        doc("b", "green hat", "red hat", "y"),
        doc("gone", "red", "red", "x"),
    ] {
        db.put("c", &d, Durability::Fsync).unwrap();
    }
    db.commit().unwrap();
    db.delete("c", "gone", Durability::Fsync).unwrap();
    db.commit().unwrap();
    db.put("c", &doc("c", "red", "green shoes", "y"), Durability::Fsync)
        .unwrap();
    Fixture {
        db,
        title,
        body,
        tag,
    }
}

fn term(field: FieldId, t: &str) -> Query {
    Query::Term(TermQuery {
        field,
        term: t.into(),
    })
}

/// Every hit of `query` with its explanation, best first; the explanation's score is the hit's, bit for bit, under
/// every strategy.
fn explained(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
    query: &Query,
) -> Vec<(String, Located, f64, Explanation)> {
    let views = c.views(memtable).unwrap();
    let plan =
        SearchPlan::with_terms(query, c.schema(), &c.search_statistics(memtable), &views).unwrap();
    let request = SearchRequest {
        limit: 100,
        explain: true,
        ..SearchRequest::default()
    };
    let mut first = None;
    for pruning in STRATEGIES {
        let page = views.search(&plan, &request, pruning).unwrap();
        let hits: Vec<_> = page
            .hits
            .into_iter()
            .map(|h| {
                let e = h.explanation.expect("explain: true explains every hit");
                assert_eq!(e.score().to_bits(), h.score.to_bits(), "{pruning:?}");
                (h.document.id().to_owned(), h.location, h.score, e)
            })
            .collect();
        match &first {
            None => first = Some(hits),
            Some(f) => assert_eq!(f, &hits, "{pruning:?}"),
        }
    }
    first.unwrap()
}

fn word(e: &Explanation) -> &WordExplanation {
    match e {
        Explanation::Word(w) => w,
        other => panic!("not a word: {other:?}"),
    }
}

fn close(a: f64, b: f64) {
    assert!((a - b).abs() <= 1e-12 * b.abs().max(1.0), "{a} vs {b}");
}

#[test]
fn a_combined_word_by_hand() {
    let f = fixture();
    let snapshot = f.db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let red = Query::Combined(CombinedQuery {
        fields: vec![
            WeightedTerm {
                field: f.title,
                term: "red".into(),
                weight: 2.0,
            },
            WeightedTerm {
                field: f.body,
                term: "red".into(),
                weight: 1.0,
            },
        ],
    });
    let hits = explained(c, &memtable, &red);
    let ids: Vec<_> = hits.iter().map(|h| h.0.as_str()).collect();
    assert_eq!(ids.len(), 3);
    for id in ["a", "b", "c"] {
        assert!(ids.contains(&id), "{ids:?}");
    }

    // Counted: a, b, gone (deleted, still in its segment) and c; N = 4. `red` titles: a, gone, c; bodies: a, b, gone.
    let idf = (1.0_f64 + (4.0 - 3.0 + 0.5) / (3.0 + 0.5)).ln();
    let avg_title = (2.0 + 2.0 + 1.0 + 1.0) / 4.0;
    let avg_body = (4.0 + 2.0 + 1.0 + 2.0) / 4.0;
    let factor = |w: f64, len: f64, avg: f64| w / (1.0 - 0.75 + 0.75 * len / avg);

    let a = word(&hits.iter().find(|h| h.0 == "a").unwrap().3);
    close(a.idf, idf);
    assert_eq!(a.k1, 1.2);
    assert_eq!(a.fields.len(), 2);
    let (t, b) = (&a.fields[0], &a.fields[1]);
    assert_eq!(
        (t.field, t.term.as_slice(), t.tf, t.length),
        (f.title, &b"red"[..], 1, Some(2))
    );
    assert_eq!(
        (b.field, b.term.as_slice(), b.tf, b.length),
        (f.body, &b"red"[..], 2, Some(4))
    );
    close(t.factor, factor(2.0, 2.0, avg_title));
    close(b.factor, factor(1.0, 4.0, avg_body));
    assert_eq!(t.contribution, t.factor);
    assert_eq!(b.contribution, 2.0 * b.factor);
    assert_eq!(a.tf, t.contribution + b.contribution);
    close(a.score, idf * a.tf * 2.2 / (1.2 + a.tf));

    // `b` holds `red` in its body only; `c` (memtable) in its title only.
    let b = word(&hits.iter().find(|h| h.0 == "b").unwrap().3);
    assert_eq!(
        b.fields.iter().map(|x| x.field).collect::<Vec<_>>(),
        [f.body]
    );
    let (_, location, _, e) = hits.iter().find(|h| h.0 == "c").unwrap();
    assert!(matches!(location, Located::Memtable(_)));
    assert_eq!(
        word(e).fields.iter().map(|x| x.field).collect::<Vec<_>>(),
        [f.title]
    );
}

#[test]
fn a_keyword_has_no_length() {
    let f = fixture();
    let snapshot = f.db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let hits = explained(c, &memtable, &term(f.tag, "x"));
    assert_eq!(hits.len(), 1);
    let w = word(&hits[0].3);
    let field = &w.fields[0];
    assert_eq!((field.tf, field.norm, field.length), (1, 0, None));
    // Weight 1, b = 0: the factor is 1 and a single value scores idf.
    assert_eq!(field.factor, 1.0);
    close(w.score, w.idf);
}

#[test]
fn bools_list_what_matched() {
    let f = fixture();
    let snapshot = f.db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let query = Query::Bool(BoolQuery {
        must: vec![term(f.body, "red")],
        should: vec![term(f.body, "hat"), term(f.body, "running")],
        must_not: vec![term(f.title, "blue")],
        filter: vec![Filter::Term {
            field: f.tag,
            value: FilterValue::Keyword("x".into()),
        }],
    });
    let hits = explained(c, &memtable, &query);
    // `b` holds `red` but its tag is `y`; `c` lacks `red` in its body.
    assert_eq!(hits.len(), 1);
    let (id, _, score, e) = &hits[0];
    assert_eq!(id, "a");
    let Explanation::Bool {
        score: s,
        must,
        should,
    } = e
    else {
        panic!("a bool: {e:?}")
    };
    assert_eq!(s.to_bits(), score.to_bits());
    assert_eq!(must.len(), 1);
    // `hat` does not match `a`: only `running` is listed.
    assert_eq!(should.len(), 1);
    assert_eq!(word(&should[0]).fields[0].term, b"running");
    assert_eq!(
        score.to_bits(),
        (must[0].score() + should[0].score()).to_bits()
    );

    // Only a filter and an exclusion: a match scoring 0.
    let only = Query::Bool(BoolQuery {
        must_not: vec![term(f.body, "hat")],
        ..BoolQuery::default()
    });
    let hits = explained(c, &memtable, &only);
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.3 == Explanation::All));
}

#[test]
fn phrases_boosts_and_constants() {
    let f = fixture();
    let snapshot = f.db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let phrase = Query::Phrase(PhraseQuery {
        field: f.body,
        terms: ["running", "shoes"]
            .into_iter()
            .zip(0..)
            .map(|(t, position)| PhraseTerm {
                term: t.into(),
                position,
            })
            .collect(),
    });
    let hits = explained(c, &memtable, &phrase);
    assert_eq!(hits.len(), 1);
    let Explanation::Phrase(p) = &hits[0].3 else {
        panic!("a phrase")
    };
    assert_eq!((p.field, p.freq, p.length), (f.body, 1, 4), "{p:?}");
    assert_eq!(p.terms, [b"running".to_vec(), b"shoes".to_vec()]);
    assert_eq!(p.tf, p.factor);

    let boosted = Query::Boost(BoostQuery {
        query: Box::new(term(f.body, "hat")),
        boost: 3.0,
    });
    let hits = explained(c, &memtable, &boosted);
    let Explanation::Boost {
        score,
        boost,
        query,
    } = &hits[0].3
    else {
        panic!("a boost")
    };
    assert_eq!(
        (*boost, score.to_bits()),
        (3.0, (query.score() * 3.0).to_bits())
    );

    let constant = Query::ConstantScore(ConstantScoreQuery {
        query: Box::new(term(f.body, "hat")),
        score: 5.0,
    });
    let hits = explained(c, &memtable, &constant);
    assert!(
        matches!(&hits[0].3, Explanation::Constant { score, query } if *score == 5.0 && matches!(**query, Explanation::Word(_)))
    );
}

#[test]
fn expansions_show_their_terms() {
    let f = fixture();
    let snapshot = f.db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let prefix = Query::Prefix(PrefixQuery {
        field: f.body,
        prefix: "r".into(),
        max_expansions: 50,
    });
    // `red` and `running` are kept; `a` holds both, so both are listed under a `bool`.
    let hits = explained(c, &memtable, &prefix);
    let (_, _, _, e) = hits.iter().find(|h| h.0 == "a").unwrap();
    let Explanation::Bool { must, should, .. } = e else {
        panic!("a bool: {e:?}")
    };
    assert_eq!(must, &[]);
    let terms: Vec<_> = should
        .iter()
        .map(|s| word(s).fields[0].term.clone())
        .collect();
    assert_eq!(terms, [b"red".to_vec(), b"running".to_vec()]);
    // One shared idf.
    assert_eq!(word(&should[0]).idf, word(&should[1]).idf);

    // `shoez` is one edit from `shoes`: weight (1 + 1 − 1) / 2.
    let fuzzy = Query::Fuzzy(FuzzyQuery {
        field: f.body,
        term: "shoez".into(),
        distance: 1,
        max_expansions: 50,
    });
    let hits = explained(c, &memtable, &fuzzy);
    assert_eq!(hits.len(), 2);
    for (_, _, _, e) in &hits {
        let Explanation::Boost { boost, query, .. } = e else {
            panic!("a weighted term: {e:?}")
        };
        assert_eq!(*boost, 0.5);
        assert_eq!(word(query).fields[0].term, b"shoes");
    }
}

#[test]
fn what_does_not_match_is_not_explained() {
    let f = fixture();
    let snapshot = f.db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let stats = c.search_statistics(&memtable);
    let segment = views.segments().next().unwrap();
    // The segment holds a, b, gone at docids 0, 1, 2 (`gone` deleted).
    let ids: Vec<_> = (0..3).map(|d| c.segments()[0].id_of(d).unwrap()).collect();
    assert_eq!(ids, ["a", "b", "gone"]);

    let red = SearchPlan::new(&term(f.body, "red"), c.schema(), &stats).unwrap();
    assert!(red.explain(&segment, 0).unwrap().is_some());
    assert!(red.explain(&segment, 1).unwrap().is_some());
    assert!(red.explain(&segment, 2).unwrap().is_none(), "deleted");
    assert!(
        red.explain(&segment, 99).unwrap().is_none(),
        "past the view"
    );
    let all = SearchPlan::new(&Query::MatchAll, c.schema(), &stats).unwrap();
    assert!(all.explain(&segment, 2).unwrap().is_none(), "deleted");
    assert!(
        all.explain(&segment, 99).unwrap().is_none(),
        "past the view"
    );
    let not_red = Query::Bool(BoolQuery {
        must: vec![term(f.body, "running")],
        must_not: vec![term(f.body, "red")],
        ..BoolQuery::default()
    });
    let not_red = SearchPlan::new(&not_red, c.schema(), &stats).unwrap();
    assert!(not_red.explain(&segment, 0).unwrap().is_none(), "excluded");
    let tagged = |tag: &str| {
        let q = Query::Bool(BoolQuery {
            must: vec![term(f.body, "red")],
            filter: vec![Filter::Term {
                field: f.tag,
                value: FilterValue::Keyword(tag.into()),
            }],
            ..BoolQuery::default()
        });
        SearchPlan::new(&q, c.schema(), &stats).unwrap()
    };
    assert!(tagged("x").explain(&segment, 0).unwrap().is_some());
    assert!(
        tagged("y").explain(&segment, 0).unwrap().is_none(),
        "filtered out"
    );
    // A `should` is required without `must` or `filter`.
    let either = Query::Bool(BoolQuery {
        should: vec![term(f.body, "hat"), term(f.title, "blue")],
        ..BoolQuery::default()
    });
    let either = SearchPlan::new(&either, c.schema(), &stats).unwrap();
    assert!(either.explain(&segment, 0).unwrap().is_none(), "no should");
    assert!(either.explain(&segment, 1).unwrap().is_some());

    // Without `explain`, hits carry none.
    let page = views
        .search(&red, &SearchRequest::default(), Pruning::default())
        .unwrap();
    assert_ne!(page.hits.len(), 0);
    assert!(page.hits.iter().all(|h| h.explanation.is_none()));
}

#[test]
fn a_replaced_copy_is_not_explained() {
    let mut f = fixture();
    // `b` replaced in the memtable: its segment copy is hidden.
    let doc = Document::new("b")
        .with(f.title, Value::String("blue".into()))
        .with(f.body, Value::String("red".into()));
    f.db.put("c", &doc, Durability::Fsync).unwrap();
    let snapshot = f.db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let plan = SearchPlan::new(
        &term(f.body, "red"),
        c.schema(),
        &c.search_statistics(&memtable),
    )
    .unwrap();
    let segment = views.segments().next().unwrap();
    assert_eq!(c.segments()[0].id_of(1).unwrap(), "b");
    assert!(plan.explain(&segment, 1).unwrap().is_none());
    let hits = explained(c, &memtable, &term(f.body, "red"));
    let b = hits.iter().find(|h| h.0 == "b").unwrap();
    assert!(matches!(b.1, Located::Memtable(_)));
}

#[test]
fn a_filter_beside_should_clauses() {
    let f = fixture();
    let snapshot = f.db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    // No `must`: the filter selects `b` and `c` (tag `y`), and the `should` clauses only score.
    let query = Query::Bool(BoolQuery {
        should: vec![term(f.body, "hat"), term(f.body, "running")],
        filter: vec![Filter::Term {
            field: f.tag,
            value: FilterValue::Keyword("y".into()),
        }],
        ..BoolQuery::default()
    });
    let hits = explained(c, &memtable, &query);
    assert_eq!(hits.len(), 2);
    let (_, _, score, b) = hits.iter().find(|h| h.0 == "b").unwrap();
    let Explanation::Bool { must, should, .. } = b else {
        panic!("a bool: {b:?}")
    };
    assert_eq!((must.len(), should.len()), (0, 1));
    assert_eq!(score.to_bits(), should[0].score().to_bits());
    // `c` matches no `should`: selected, scoring 0.
    let (_, _, score, e) = hits.iter().find(|h| h.0 == "c").unwrap();
    assert_eq!((e, score.to_bits()), (&Explanation::All, 0.0_f64.to_bits()));
}

#[test]
fn one_pass_explains_repeats_deleted_and_excluded_documents_as_each_alone() {
    let f = fixture();
    let snapshot = f.db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let stats = c.search_statistics(&memtable);
    // a, b, gone at docids 0, 1, 2; `gone` deleted, and b hidden by the memtable's copy.
    let segment = views.segments().next().unwrap();
    // A nested `bool` whose `must_not` short-circuits on some documents, beside a filter and a `should`.
    let query = Query::Bool(BoolQuery {
        must: vec![Query::Bool(BoolQuery {
            should: vec![term(f.body, "red"), term(f.title, "shoes")],
            must_not: vec![term(f.body, "hat")],
            ..BoolQuery::default()
        })],
        should: vec![term(f.body, "running")],
        filter: vec![Filter::Term {
            field: f.tag,
            value: FilterValue::Keyword("x".into()),
        }],
        ..BoolQuery::default()
    });
    for query in [query, term(f.body, "red"), Query::MatchAll] {
        let plan = SearchPlan::new(&query, c.schema(), &stats).unwrap();
        // Repeats, a deleted docid between live ones, one past the view, out of order.
        let docids = [1, 0, 0, 2, 1, 99, 0, 2];
        let together = plan.explain_all(&segment, &docids).unwrap();
        for (&docid, e) in docids.iter().zip(&together) {
            assert_eq!(e, &plan.explain(&segment, docid).unwrap(), "docid {docid}");
        }
        assert!(together[1].is_some(), "a matches {query:?}");
    }
}

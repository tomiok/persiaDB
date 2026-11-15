//! `explain: true` states which relaxation step matched each hit (ROADMAP 8.3.4, SPEC §7.7): a [`RelaxedExplanation`]
//! at the root, with the hit's level, step and its step's `applied`, around the step's own tree, whose score is the
//! hit's bit for bit. It holds at every level, L5's static rank included, on cursor pages, and past a skipped step.

use persia_storage::container::sim::SimFile;

use super::{Stub, exec, step, term};
use crate::db::relaxed::{RelaxedHit, RelaxedResults};
use crate::db::{Database, DatabaseOptions, SearchRequest};
use crate::document::{Document, Value};
use crate::query::{BoolQuery, Query, QueryLimits};
use crate::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use crate::schema::{FieldId, FieldKind, RelaxationConfig, SchemaBuilder, TextOptions};
use crate::search::{Explanation, Projection, RelaxedExplanation};
use crate::wal::Durability;

const WORDS: [&str; 8] = [
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
];

/// One document per word of [`WORDS`] (`d0` holds `alpha`), over a segment and the memtable.
fn database() -> (Database<SimFile>, FieldId) {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    for (i, word) in WORDS.into_iter().enumerate() {
        let doc = Document::new(format!("d{i}")).with(body, Value::String(word.into()));
        db.put("c", &doc, Durability::Buffered).unwrap();
        if i == 3 {
            db.commit().unwrap();
        }
    }
    (db, body)
}

/// The collection's levels with `fill`, L1 to L4 replaced by steps of one word each (L2 two of them), and the engine's
/// own L5: every level finds hits.
fn every_level(body: FieldId) -> RelaxPlan {
    let options = RelaxOptions {
        fill: Some(true),
        ..RelaxOptions::default()
    };
    RelaxPlan::new(&RelaxationConfig::default(), &options)
        .unwrap()
        .with_level(
            RelaxLevel::Synonyms,
            Stub::new(vec![step(term(body, "beta"), "synonyms")]),
        )
        .with_level(
            RelaxLevel::DropTerms,
            Stub::new(vec![
                step(term(body, "gamma"), "drop_terms:75%"),
                step(term(body, "delta"), "drop_terms:1"),
            ]),
        )
        .with_level(
            RelaxLevel::Prefix,
            Stub::new(vec![step(term(body, "epsilon"), "prefix")]),
        )
        .with_level(
            RelaxLevel::Fuzzy,
            Stub::new(vec![step(term(body, "zeta"), "fuzzy")]),
        )
}

fn explained(limit: usize) -> SearchRequest {
    SearchRequest {
        limit,
        explain: true,
        projection: Projection::None,
        ..SearchRequest::default()
    }
}

/// Every hit's explanation states its own level and step, its step's `applied` (none at L0), and scores it bit for bit.
fn check(out: &RelaxedResults) {
    for RelaxedHit {
        hit, level, step, ..
    } in &out.hits
    {
        let Some(Explanation::Relaxed(RelaxedExplanation {
            level: l,
            step: s,
            applied,
            explanation,
        })) = &hit.explanation
        else {
            panic!("{}: {:?}", hit.document.id(), hit.explanation)
        };
        assert_eq!((l, s), (level, step), "{}", hit.document.id());
        let expected = step
            .checked_sub(1)
            .map(|i| out.relaxation.applied[i].clone());
        assert_eq!(applied, &expected, "{}", hit.document.id());
        let root = hit.explanation.as_ref().unwrap();
        assert_eq!(root.score().to_bits(), hit.score.to_bits());
        assert_eq!(explanation.score().to_bits(), hit.score.to_bits());
        assert!(
            !matches!(**explanation, Explanation::Relaxed(_)),
            "wrapped twice"
        );
        assert_eq!(root.scoring(), &**explanation);
        // L5's step is explained as its static rank; every other step as its query's tree.
        assert_eq!(
            *level == RelaxLevel::Fallback,
            matches!(**explanation, Explanation::StaticRank { .. })
        );
    }
}

#[test]
fn every_level_states_its_step_and_applied_around_its_tree() {
    let (db, body) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    let relax = every_level(body);
    let alpha = term(body, "alpha");
    let out = views
        .search_relaxed(&alpha, &explained(10), &relax, &exec())
        .unwrap();
    let levels: Vec<(RelaxLevel, usize)> = out.hits.iter().map(|h| (h.level, h.step)).collect();
    assert_eq!(
        levels,
        [
            (RelaxLevel::Exact, 0),
            (RelaxLevel::Synonyms, 1),
            (RelaxLevel::DropTerms, 2),
            (RelaxLevel::DropTerms, 3),
            (RelaxLevel::Prefix, 4),
            (RelaxLevel::Fuzzy, 5),
            (RelaxLevel::Fallback, 6),
            (RelaxLevel::Fallback, 6),
        ]
    );
    check(&out);
    // Cursor pages state the same, each of its hits' own step, from the cursor's step on.
    let mut request = explained(3);
    let mut paged = Vec::new();
    // 8 hits by 3: three pages, then an empty one. A cursor that stops advancing fails the bound instead of hanging.
    let mut finished = false;
    for _ in 0..=out.hits.len() {
        let page = views
            .search_relaxed(&alpha, &request, &relax, &exec())
            .unwrap();
        check(&page);
        let Some(last) = page.hits.last() else {
            finished = true;
            break;
        };
        request.search_after = Some(last.cursor());
        paged.extend(page.hits);
    }
    assert!(
        finished,
        "the cursor walk did not end: {} hits",
        paged.len()
    );
    // The same hits and explanations; their cursors' last step grows page by page under `fill`.
    let shown = |hits: &[RelaxedHit]| -> Vec<(String, usize, Option<Explanation>)> {
        hits.iter()
            .map(|h| {
                (
                    h.hit.document.id().to_owned(),
                    h.step,
                    h.hit.explanation.clone(),
                )
            })
            .collect()
    };
    assert_eq!(shown(&paged), shown(&out.hits));
    // Without `explain`, no hit has an explanation.
    let quiet = views
        .search_relaxed(
            &alpha,
            &SearchRequest {
                explain: false,
                ..explained(10)
            },
            &relax,
            &exec(),
        )
        .unwrap();
    assert!(quiet.hits.iter().all(|h| h.hit.explanation.is_none()));
}

#[test]
fn a_step_after_a_skipped_one_states_its_own_applied() {
    let (db, body) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // L1's step needs 20 terms, over the 10 allowed: it is skipped and listed, and `beta` falls to L5.
    let wide = Query::Bool(BoolQuery {
        should: (0..20).map(|i| term(body, &format!("w{i}"))).collect(),
        ..BoolQuery::default()
    });
    let relax = every_level(body)
        .with_level(
            RelaxLevel::Synonyms,
            Stub::new(vec![step(wide, "synonyms")]),
        )
        .with_limits(QueryLimits {
            max_terms: 10,
            ..QueryLimits::default()
        });
    let out = views
        .search_relaxed(&term(body, "alpha"), &explained(10), &relax, &exec())
        .unwrap();
    assert_eq!(out.relaxation.applied[0], "synonyms:skipped");
    check(&out);
    let gamma = out
        .hits
        .iter()
        .find(|h| h.hit.document.id() == "d2")
        .unwrap();
    assert!(matches!(
        &gamma.hit.explanation,
        Some(Explanation::Relaxed(r)) if r.step == 2 && r.applied.as_deref() == Some("drop_terms:75%")
    ));
    let beta = out
        .hits
        .iter()
        .find(|h| h.hit.document.id() == "d1")
        .unwrap();
    assert_eq!(beta.level, RelaxLevel::Fallback);
    assert!(matches!(
        &beta.hit.explanation,
        Some(Explanation::Relaxed(r)) if r.applied.as_deref() == Some("fallback")
    ));
}

#[test]
fn a_cut_step_states_its_cut_applied() {
    let (db, body) = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let memtable = c.memtable();
    let views = c.views(&memtable).unwrap();
    // L3's step is a `prefix` of 50 expansions, over the 10 allowed: it is cut to 10 and still finds `epsilon`.
    let eps = Query::Prefix(crate::query::PrefixQuery {
        field: body,
        prefix: "eps".into(),
        max_expansions: 50,
    });
    let relax = every_level(body)
        .with_level(RelaxLevel::Prefix, Stub::new(vec![step(eps, "prefix")]))
        .with_limits(QueryLimits {
            max_expansions: 10,
            ..QueryLimits::default()
        });
    let out = views
        .search_relaxed(&term(body, "alpha"), &explained(10), &relax, &exec())
        .unwrap();
    check(&out);
    let epsilon = out
        .hits
        .iter()
        .find(|h| h.hit.document.id() == "d4")
        .unwrap();
    assert!(matches!(
        &epsilon.hit.explanation,
        Some(Explanation::Relaxed(r)) if r.level == RelaxLevel::Prefix && r.applied.as_deref() == Some("prefix:cut")
    ));
}

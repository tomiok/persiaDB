//! Hand-computed BM25F examples (ROADMAP 7.3.4) and the bound property (ROADMAP 7.3.3).
#![allow(clippy::cast_precision_loss, clippy::unnecessary_wraps)] // Tests: generated numbers; `hit` builds `Option` hits.

use std::collections::HashMap;

use proptest::prelude::*;

use super::*;
use crate::query::WeightedTerm;
use crate::schema::{KeywordOptions, SchemaBuilder, TextOptions};
use crate::stats::FieldTotals;

#[derive(Default)]
struct Stats {
    n: u64,
    fields: HashMap<FieldId, FieldTotals>,
    df: HashMap<(FieldId, Vec<u8>), u64>,
}

impl StatisticsProvider for Stats {
    fn doc_count(&self) -> u64 {
        self.n
    }

    fn field(&self, field: FieldId) -> FieldTotals {
        self.fields.get(&field).copied().unwrap_or_default()
    }

    fn df(&self, field: FieldId, term: &[u8]) -> Result<u64, ReadError> {
        Ok(self.df.get(&(field, term.to_vec())).copied().unwrap_or(0))
    }
}

fn totals(docs_with_field: u64, sum_field_len: u64) -> FieldTotals {
    FieldTotals {
        docs_with_field,
        sum_field_len,
    }
}

fn text(boost: f64, k1: f64, b: f64) -> FieldKind {
    FieldKind::Text(TextOptions {
        boost,
        k1,
        b,
        ..TextOptions::default()
    })
}

fn hit(tf: u32, len: u32) -> Option<FieldHit> {
    Some(FieldHit {
        tf,
        norm: norms::encode(len),
    })
}

fn close(a: f64, b: f64) {
    assert!((a - b).abs() <= 1e-12 * b.abs().max(1.0), "{a} != {b}");
}

fn schema() -> (CollectionSchema, FieldId, FieldId, FieldId) {
    let s = SchemaBuilder::new("c")
        .field("title", text(3.0, 1.2, 0.75))
        .field("body", text(1.0, 1.2, 0.75))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let id = |n| s.field(n).unwrap().id();
    let (t, b, k) = (id("title"), id("body"), id("tag"));
    (s, t, b, k)
}

#[test]
fn idf_by_hand() {
    close(idf(10, 2), 4.4_f64.ln());
    close(idf(0, 0), 2_f64.ln());
    close(idf(1, 1), (1.0 + 0.5 / 1.5_f64).ln());
    // A df above N (statistics of a damaged segment) is N: idf stays positive.
    assert_eq!(idf(3, 7), idf(3, 3));
    assert!(idf(u64::MAX, u64::MAX) > 0.0);
}

#[test]
fn single_text_field_by_hand() {
    let (s, title, _, _) = schema();
    let stats = Stats {
        n: 10,
        fields: HashMap::from([(title, totals(10, 40))]),
        df: HashMap::from([((title, b"shoe".to_vec()), 2)]),
    };
    let query = TermQuery {
        field: title,
        term: "shoe".into(),
    };
    let scorer = TermScorer::for_term(&query, &s, &stats).unwrap();
    let idf = 4.4_f64.ln();
    close(scorer.idf(), idf);
    // The title's boost 3 is its weight: len = avglen gives tf~ = 3 · tf.
    // tf 1: tf~ = 3, score = idf · 3 · 2.2 / 4.2.
    close(scorer.score(&[hit(1, 4)]), idf * 6.6 / 4.2);
    // tf 2, len 8 = 2 · avglen: denominator 1 − 0.75 + 1.5 = 1.75, tf~ = 6 / 1.75 = 24/7;
    // score = idf · (24/7 · 2.2) / (1.2 + 24/7) = idf · 52.8 / 32.4.
    close(scorer.score(&[hit(2, 8)]), idf * 52.8 / 32.4);
    close(scorer.max_score(), idf * 2.2);
    assert_eq!(scorer.score(&[None]), 0.0);
    assert_eq!(scorer.score(&[]), 0.0);
    assert_eq!(scorer.score(&[Some(FieldHit { tf: 0, norm: 4 })]), 0.0);
}

#[test]
fn combined_by_hand() {
    let (s, title, body, _) = schema();
    let stats = Stats {
        n: 100,
        fields: HashMap::from([(title, totals(100, 400)), (body, totals(50, 500))]),
        df: HashMap::from([
            ((title, b"run".to_vec()), 5),
            ((body, b"running".to_vec()), 9),
        ]),
    };
    let query = CombinedQuery {
        fields: vec![
            WeightedTerm {
                field: title,
                term: "run".into(),
                weight: 3.0,
            },
            WeightedTerm {
                field: body,
                term: "running".into(),
                weight: 1.0,
            },
        ],
    };
    let scorer = TermScorer::for_combined(&query, &s, &stats).unwrap();
    assert_eq!(scorer.fields(), [title, body]);
    // df is the larger of the fields' (SPEC §7.2).
    let idf = (1.0 + 91.5 / 9.5_f64).ln();
    close(scorer.idf(), idf);
    // Title tf 1 at its avglen 4: 3 · 1 = 3. Body tf 2 at its avglen 10: 1 · 2 = 2. tf~ = 5.
    close(scorer.tf(&[hit(1, 4), hit(2, 10)]), 5.0);
    close(
        scorer.score(&[hit(1, 4), hit(2, 10)]),
        idf * 5.0 * 2.2 / 6.2,
    );
    // Body only: tf~ = 2.
    close(scorer.score(&[None, hit(2, 10)]), idf * 2.0 * 2.2 / 3.2);
    // A field of the query's weight, not its boost.
    let mut light = query.clone();
    light.fields[0].weight = 0.5;
    let scorer = TermScorer::for_combined(&light, &s, &stats).unwrap();
    close(scorer.tf(&[hit(1, 4), None]), 0.5);
}

#[test]
fn keyword_term_scores_its_idf() {
    let (s, _, _, tag) = schema();
    let stats = Stats {
        n: 8,
        fields: HashMap::from([(tag, totals(8, 20))]),
        df: HashMap::from([((tag, b"red".to_vec()), 3)]),
    };
    let query = TermQuery {
        field: tag,
        term: "red".into(),
    };
    let scorer = TermScorer::for_term(&query, &s, &stats).unwrap();
    let idf = (1.0 + 5.5 / 3.5_f64).ln();
    // tf is 1 and b is 0: tf~ = 1, score = idf · 2.2 / 2.2.
    close(scorer.score(&[Some(FieldHit { tf: 1, norm: 0 })]), idf);
    assert_eq!(scorer.k1(), KEYWORD_K1);
}

#[test]
fn length_normalization_extremes() {
    let s = SchemaBuilder::new("c")
        .field("flat", text(1.0, 1.2, 0.0))
        .field("full", text(1.0, 0.0, 1.0))
        .build()
        .unwrap();
    let (flat, full) = (s.field("flat").unwrap().id(), s.field("full").unwrap().id());
    let stats = Stats {
        n: 4,
        fields: HashMap::from([(flat, totals(4, 40)), (full, totals(4, 40))]),
        df: HashMap::new(),
    };
    // b = 0: the length does not matter.
    let scorer = TermScorer::new(&[FieldParams::of(&s, flat, None).unwrap()], 1, &stats).unwrap();
    assert_eq!(scorer.score(&[hit(3, 1)]), scorer.score(&[hit(3, 30)]));
    // b = 1 and k1 = 0: the score is idf whatever tf~ is.
    let scorer = TermScorer::new(&[FieldParams::of(&s, full, None).unwrap()], 1, &stats).unwrap();
    close(scorer.score(&[hit(1, 20)]), scorer.idf());
    close(scorer.score(&[hit(9, 2)]), scorer.idf());
    // A damaged norm byte 0 with b = 1 does not divide by zero.
    let damaged = scorer.score(&[Some(FieldHit { tf: 1, norm: 0 })]);
    assert!(damaged.is_finite() && damaged >= 0.0);
}

#[test]
fn norms_are_quantized_lengths() {
    let (s, title, _, _) = schema();
    let stats = Stats {
        n: 10,
        fields: HashMap::from([(title, totals(10, 1000))]),
        df: HashMap::new(),
    };
    let scorer = TermScorer::new(&[FieldParams::of(&s, title, None).unwrap()], 1, &stats).unwrap();
    // 100 tokens is stored as the norm of 96 (SPEC §5.3.4): the score is that of 96 tokens.
    assert_eq!(norms::decode(norms::encode(100)), 96);
    assert_eq!(scorer.score(&[hit(1, 100)]), scorer.score(&[hit(1, 96)]));
    // tf~ = 3 / (0.25 + 0.75 · 96/100).
    close(scorer.tf(&[hit(1, 100)]), 3.0 / (0.25 + 0.75 * 0.96));
}

#[test]
fn k1_is_the_weighted_mean() {
    let s = SchemaBuilder::new("c")
        .field("a", text(3.0, 1.0, 0.75))
        .field("b", text(1.0, 2.0, 0.75))
        .build()
        .unwrap();
    let (a, b) = (s.field("a").unwrap().id(), s.field("b").unwrap().id());
    let stats = Stats::default();
    let p = |f| FieldParams::of(&s, f, None).unwrap();
    let scorer = TermScorer::new(&[p(a), p(b)], 0, &stats).unwrap();
    close(scorer.k1(), 1.25);
    // Equal k1s are kept exactly, whatever the weights.
    let scorer = TermScorer::new(&[p(a)], 0, &stats).unwrap();
    assert_eq!(scorer.k1(), 1.0);
}

#[test]
fn bad_parameters_are_rejected() {
    let (s, title, _, _) = schema();
    let stats = Stats::default();
    assert!(matches!(
        TermScorer::new(&[], 0, &stats),
        Err(ScoreError::NoFields)
    ));
    let good = FieldParams::of(&s, title, None).unwrap();
    for bad in [
        FieldParams {
            weight: 0.0,
            ..good
        },
        FieldParams {
            weight: 1000.5,
            ..good
        },
        FieldParams {
            weight: f64::NAN,
            ..good
        },
        FieldParams { k1: -0.1, ..good },
        FieldParams { k1: 10.5, ..good },
        FieldParams { b: 1.5, ..good },
        FieldParams {
            b: f64::NAN,
            ..good
        },
    ] {
        assert!(
            matches!(
                TermScorer::new(&[bad], 0, &stats),
                Err(ScoreError::Params { .. })
            ),
            "{bad:?}"
        );
    }
    // A keyword field cannot be part of a combined; an unknown field cannot be scored.
    let (s, _, _, tag) = schema();
    let combined = CombinedQuery {
        fields: vec![WeightedTerm {
            field: tag,
            term: "x".into(),
            weight: 1.0,
        }],
    };
    assert!(matches!(
        TermScorer::for_combined(&combined, &s, &stats),
        Err(ScoreError::Field { .. })
    ));
    let missing = FieldParams::of(&s, FieldId(99), None);
    assert!(matches!(missing, Err(ScoreError::Field { .. })));
}

fn params() -> impl Strategy<Value = (f64, f64, f64)> {
    (0.001..=1000.0_f64, 0.0..=10.0_f64, 0.0..=1.0_f64)
}

proptest! {
    /// A block's bounds (its largest tf, its smallest norm byte) bound the score of every document in it, exactly
    /// in floating point (ROADMAP 7.3.3).
    #[test]
    fn bounds_hold_bit_for_bit(
        fields in prop::collection::vec((params(), 1_u64..1000, 1_u64..100_000), 1..4),
        n in 0_u64..1_000_000,
        df in 0_u64..1_000_000,
        docs in prop::collection::vec(
            prop::collection::vec(prop::option::of((1_u32..1000, any::<u8>())), 4),
            1..20,
        ),
    ) {
        let mut stats = Stats { n, ..Stats::default() };
        let mut ps = Vec::new();
        for (i, ((weight, k1, b), docs_with_field, sum)) in fields.iter().copied().enumerate() {
            let field = FieldId(u32::try_from(i).unwrap());
            stats.fields.insert(field, totals(docs_with_field, sum));
            ps.push(FieldParams { field, weight, k1, b });
        }
        let scorer = TermScorer::new(&ps, df, &stats).unwrap();
        let hits: Vec<Vec<Option<FieldHit>>> = docs
            .iter()
            .map(|d| d.iter().map(|h| h.map(|(tf, norm)| FieldHit { tf, norm })).collect())
            .collect();
        let bound: Vec<Option<FieldHit>> = (0..4)
            .map(|i| {
                let present: Vec<FieldHit> = hits.iter().filter_map(|h| h[i]).collect();
                let max_tf = present.iter().map(|h| h.tf).max()?;
                let min_norm = present.iter().map(|h| h.norm).min()?;
                Some(FieldHit { tf: max_tf, norm: min_norm })
            })
            .collect();
        let upper = scorer.score(&bound);
        prop_assert!(upper.is_finite() && upper <= scorer.max_score());
        for h in &hits {
            let s = scorer.score(h);
            prop_assert!(s.is_finite() && s >= 0.0);
            prop_assert!(s <= upper, "{s} > {upper} for {h:?}");
        }
    }
}

/// The textbook form `idf · tf · (k1+1) / (k1+tf)` is not monotonic in floating point: adjacent `tf~` values can
/// round to decreasing scores, which would let a document's score exceed its block's bound. The form the scorer
/// uses is monotonic step by step: checked here over adjacent floats, where rounding decides.
#[test]
fn score_is_monotonic_across_adjacent_floats() {
    let (s, title, _, _) = schema();
    let mut rng = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for _ in 0..200 {
        let k1 = (next() % 10_001) as f64 / 1000.0;
        let n = next() % 1_000_000;
        let df = next() % (n + 1);
        let mut params = FieldParams::of(&s, title, None).unwrap();
        params.k1 = k1;
        let scorer = TermScorer::new(
            &[params],
            df,
            &Stats {
                n,
                ..Stats::default()
            },
        )
        .unwrap();
        let mut tf = (next() % 1_000_000) as f64 / 997.0 + f64::MIN_POSITIVE;
        for _ in 0..2000 {
            let up = tf.next_up();
            assert!(
                scorer.score_tf(tf) <= scorer.score_tf(up),
                "k1 {k1}, N {n}, df {df}: score({tf}) > score({up})"
            );
            tf = up;
        }
    }
}

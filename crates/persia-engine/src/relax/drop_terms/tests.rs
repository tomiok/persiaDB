#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::float_cmp)] // Tests: inputs are fixed; IDFs are exact.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::query::{
    BoostQuery, CombinedQuery, ConstantScoreQuery, PhraseQuery, PhraseTerm, PrefixQuery, TermQuery,
    WeightedTerm,
};
use crate::schema::FieldId;
use crate::stats::FieldTotals;

/// 100 documents; each term's `df` as given, 0 for any other.
struct Stats(BTreeMap<(u32, &'static str), u64>);

impl StatisticsProvider for Stats {
    fn doc_count(&self) -> u64 {
        100
    }
    fn field(&self, _: FieldId) -> FieldTotals {
        FieldTotals::default()
    }
    fn df(&self, field: FieldId, term: &[u8]) -> Result<u64, ReadError> {
        let term = std::str::from_utf8(term).unwrap();
        Ok(self
            .0
            .iter()
            .find(|((f, t), _)| FieldId::new(*f) == field && *t == term)
            .map_or(0, |(_, df)| *df))
    }
}

fn stats() -> Stats {
    Stats(BTreeMap::from([
        ((0, "red"), 50),
        ((0, "shoes"), 10),
        ((1, "shoes"), 30),
        ((0, "nike"), 2),
    ]))
}

fn term(field: u32, term: &str) -> Query {
    Query::Term(TermQuery {
        field: FieldId::new(field),
        term: term.into(),
    })
}

fn importance(stopword: bool, idf: f64, position: usize) -> Importance {
    Importance {
        stopword,
        idf,
        position,
    }
}

#[test]
fn stopwords_drop_first_then_lower_idf_then_later_position() {
    let terms = [
        importance(false, 2.0, 0),
        importance(true, 9.0, 1),
        importance(false, 1.0, 2),
        importance(false, 2.0, 3),
        importance(true, 0.5, 4),
    ];
    // Stopwords first (lower IDF first among them), then IDF 1.0, then the two at 2.0, the later one first.
    assert_eq!(drop_order(&terms), [4, 1, 2, 3, 0]);
    assert_eq!(drop_order(&[]), Vec::<usize>::new());
}

proptest! {
    /// The order is a permutation in which no term is more important than one dropped after it.
    #[test]
    fn drop_order_never_drops_a_more_important_term_first(
        terms in prop::collection::vec((any::<bool>(), 0_u8..4), 0..12),
    ) {
        let terms: Vec<Importance> = terms
            .iter()
            .enumerate()
            .map(|(position, &(stopword, idf))| importance(stopword, f64::from(idf), position))
            .collect();
        let order = drop_order(&terms);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        prop_assert_eq!(sorted, (0..terms.len()).collect::<Vec<_>>());
        for w in order.windows(2) {
            let (a, b) = (terms[w[0]], terms[w[1]]);
            prop_assert_eq!(a.cmp(&b), Ordering::Less, "{:?} before {:?}", a, b);
            prop_assert!(a.stopword || !b.stopword, "a stopword after a term that is not one");
            if a.stopword == b.stopword {
                prop_assert!(a.idf < b.idf || (a.idf == b.idf && a.position > b.position));
            }
        }
    }
}

#[test]
fn query_idf_is_the_idf_bm25f_scores_with() {
    let stats = stats();
    let n = 100;
    assert_eq!(query_idf(&term(0, "red"), &stats).unwrap(), idf(n, 50));
    assert_eq!(query_idf(&term(0, "absent"), &stats).unwrap(), idf(n, 0));
    // `combined` takes the largest `df` of its terms.
    let combined = Query::Combined(CombinedQuery {
        fields: [(0, 3.0), (1, 1.0)]
            .map(|(f, weight)| WeightedTerm {
                field: FieldId::new(f),
                term: "shoes".into(),
                weight,
            })
            .into(),
    });
    assert_eq!(query_idf(&combined, &stats).unwrap(), idf(n, 30));
    // A phrase sums its terms'.
    let phrase = Query::Phrase(PhraseQuery {
        field: FieldId::new(0),
        terms: ["red", "shoes"]
            .iter()
            .zip(0..)
            .map(|(t, position)| PhraseTerm {
                term: (*t).into(),
                position,
            })
            .collect(),
    });
    assert_eq!(query_idf(&phrase, &stats).unwrap(), idf(n, 50) + idf(n, 10));
    // A bool sums `must` and adds its best `should`; `must_not` and `filter` do not score.
    let bool = Query::Bool(BoolQuery {
        must: vec![term(0, "red"), term(0, "shoes")],
        should: vec![term(0, "red"), term(0, "nike")],
        must_not: vec![term(0, "absent")],
        ..BoolQuery::default()
    });
    assert_eq!(
        query_idf(&bool, &stats).unwrap(),
        idf(n, 50) + idf(n, 10) + idf(n, 2)
    );
    // `boost` and `constant_score` are their query's; `prefix` and `match_all` have none.
    let boosted = Query::Boost(BoostQuery {
        query: Box::new(term(0, "nike")),
        boost: 5.0,
    });
    assert_eq!(query_idf(&boosted, &stats).unwrap(), idf(n, 2));
    let constant = Query::ConstantScore(ConstantScoreQuery {
        query: Box::new(term(0, "nike")),
        score: 1.0,
    });
    assert_eq!(query_idf(&constant, &stats).unwrap(), idf(n, 2));
    let prefix = Query::Prefix(PrefixQuery {
        field: FieldId::new(0),
        prefix: "ni".into(),
        max_expansions: 50,
    });
    assert_eq!(query_idf(&prefix, &stats).unwrap(), 0.0);
    assert_eq!(query_idf(&Query::MatchAll, &stats).unwrap(), 0.0);
}

#[test]
fn query_idf_reports_a_df_it_cannot_read() {
    struct Broken;
    impl StatisticsProvider for Broken {
        fn doc_count(&self) -> u64 {
            1
        }
        fn field(&self, _: FieldId) -> FieldTotals {
            FieldTotals::default()
        }
        fn df(&self, field: FieldId, _: &[u8]) -> Result<u64, ReadError> {
            Err(ReadError::UnknownField { field })
        }
    }
    let nested = Query::Bool(BoolQuery {
        should: vec![term(0, "red")],
        ..BoolQuery::default()
    });
    assert!(query_idf(&nested, &Broken).is_err());
}

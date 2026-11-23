//! Relaxed queries within [`QueryLimits`] (SPEC §7.1.3, §7.6; ROADMAP 8.1.8): a step of the relaxation cascade runs
//! `bool { must: [step], must_not: [earlier steps] }`, and that whole query is held to the limits.

use crate::schema::{CollectionSchema, FieldId};

use super::QueryError;
use super::ast::Query;
use super::limits::{QueryLimits, Usage};

/// How a step fits what the limits leave once its exclusions are counted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Fit {
    /// As built.
    Fits,
    /// With its `prefix` and `fuzzy` queries' `max_expansions` cut to this query's.
    Cut(Query),
    /// Not even with every `max_expansions` at 1, or the overflow is not expansions (terms, clauses, depth).
    Skipped,
}

/// The limits a cascade's steps are held to, with the earlier steps they exclude counted (SPEC §7.1.3).
#[derive(Debug)]
pub(crate) struct StepLimits<'a> {
    schema: &'a CollectionSchema,
    limits: QueryLimits,
    /// The root `bool` and every step excluded so far, each at depth 2: what a step's own query has left.
    excluded: Usage,
    /// Every `prefix` and `fuzzy` of the steps excluded so far, under a `must_not` too: one a step's own expansion
    /// subsumes counts once ([`Self::credit`], ROADMAP 8.2.12).
    expanded: Vec<Expanded>,
    /// Whether the excluded steps alone exceed the limits on terms, clauses or depth: every later step is skipped.
    full: bool,
}

impl<'a> StepLimits<'a> {
    /// Limits for the steps after `first` (L0's query), which every one of them excludes.
    ///
    /// # Errors
    /// What [`Query::validate`] rejects in `first` other than a limit.
    pub(crate) fn new(
        schema: &'a CollectionSchema,
        limits: QueryLimits,
        first: &Query,
    ) -> Result<Self, QueryError> {
        first.check(schema, &QueryLimits::UNLIMITED, &mut Usage::default(), 2)?;
        let mut this = Self {
            schema,
            limits,
            excluded: Usage::default(),
            expanded: Vec::new(),
            full: false,
        };
        this.recount([first]);
        Ok(this)
    }

    /// Counts the root `bool` and `excluded` as what every later step excludes, instead of what was counted so far: a
    /// step of a nested level stands for the earlier steps of its level (ROADMAP 8.2.2.4), which no step excludes
    /// any more. Each was valid when it was first counted.
    pub(crate) fn recount<'q>(&mut self, excluded: impl IntoIterator<Item = &'q Query>) {
        self.excluded = Usage::default();
        self.expanded.clear();
        self.full = self.excluded.add(&self.limits, 1, 0, 0).is_err();
        for query in excluded {
            self.exclude(query);
        }
    }

    /// Counts `query`, searched by a step, as excluded from every later step.
    pub(crate) fn exclude(&mut self, query: &Query) {
        expanded(query, true, &mut self.expanded);
        if !self.full {
            // Over a limit, every later step is skipped. A searched step was valid (checked by `fit`, or planned as L0).
            // Expansions are counted in full but not held to their limit here: a step that subsumes some of them counts
            // them once ([`Self::credit`]), so only `fit` can tell whether a step is over it.
            let limits = QueryLimits {
                max_expansions: usize::MAX,
                ..self.limits
            };
            self.full = query
                .check(self.schema, &limits, &mut self.excluded, 2)
                .is_err();
        }
    }

    /// How `query` fits beside the exclusions counted so far.
    ///
    /// # Errors
    /// What [`Query::validate`] rejects other than a limit: a level built an invalid query. Checked first, as a check
    /// stops at its first error, which could be a limit.
    pub(crate) fn fit(&self, query: &Query) -> Result<Fit, QueryError> {
        let mut whole = Usage::default();
        query.check(self.schema, &QueryLimits::UNLIMITED, &mut whole, 2)?;
        if self.full {
            return Ok(Fit::Skipped);
        }
        if self.check(query).is_ok() {
            return Ok(Fit::Fits);
        }
        // The exclusions the step's expansions subsume as built count once ([`Self::credit`]). A cut that lowers a
        // subsuming cap below the one it subsumes counts that one again, so a cut is tried with the credit first,
        // then with none.
        let credit = self.credit(query);
        let credits: &[usize] = if credit > 0 { &[credit, 0] } else { &[0] };
        for &credit in credits {
            let mut cut = query.clone();
            let mut caps = Vec::new();
            caps_of(&mut cut, &mut caps);
            // What the caps a cut may lower share: the limit, less the exclusions' expansions (but those credited)
            // and the step's own `must_not` ones, which are not cut.
            let cuttable = caps
                .iter()
                .map(|c| usize::try_from(**c).unwrap_or(usize::MAX))
                .fold(0usize, usize::saturating_add);
            let room = self
                .limits
                .max_expansions
                .saturating_sub(self.excluded.expansions.saturating_sub(credit))
                .saturating_sub(whole.expansions.saturating_sub(cuttable));
            if caps.is_empty() || !share(&mut caps, room) {
                return Ok(Fit::Skipped);
            }
            // Over terms, clauses or depth, no cut of expansions helps.
            if self.check(&cut).is_ok() {
                return Ok(Fit::Cut(cut));
            }
        }
        Ok(Fit::Skipped)
    }

    /// The expansions of the excluded steps that `query`'s own expansions subsume (SPEC §7.1.3, ROADMAP 8.2.12): each
    /// excluded `prefix` or `fuzzy` that keeps only terms a `prefix` or `fuzzy` of `query` outside a `must_not` keeps
    /// ([`Expanded::subsumes`]), its `max_expansions` summed. Planning expands it to terms the step expands anyway, so
    /// they count once, as the step's: L4's `fuzzy` counts its own expansions, not again those of the `fuzzy:1` it
    /// excludes.
    fn credit(&self, query: &Query) -> usize {
        if self.expanded.is_empty() {
            return 0;
        }
        let mut own = Vec::new();
        expanded(query, false, &mut own);
        self.expanded
            .iter()
            .filter(|e| own.iter().any(|o| o.subsumes(e)))
            .map(|e| usize::try_from(e.cap).unwrap_or(usize::MAX))
            .fold(0usize, usize::saturating_add)
    }

    /// A step that keeps fewer synonym alternatives to fit (SPEC §7.1.3, ROADMAP 8.1.13): `counts` are how many
    /// alternatives each run has, and `build` the step's query keeping the first `keep[i]` of run `i`'s. Each run
    /// keeps the most it can in that order (least important last), the longest lists cut first, none below 1, as
    /// [`share`] cuts expansions: every list is cut to the highest level that fits, then, in run order, a list that
    /// level cut takes one more where it still fits. `None` when one each does not fit, or there is no run.
    ///
    /// Each check is of a whole query, so this costs about `log2(longest list) + runs` checks.
    pub(crate) fn cut_alternatives(
        &self,
        counts: &[usize],
        build: impl Fn(&[usize]) -> Query,
    ) -> Option<Query> {
        if self.full || counts.is_empty() {
            return None;
        }
        let at = |level: usize| -> Vec<usize> { counts.iter().map(|&n| n.min(level)).collect() };
        let fits = |keep: &[usize]| self.check(&build(keep)).is_ok();
        if !fits(&at(1)) {
            return None;
        }
        // Fewer alternatives never use more of a limit, so whether a level fits is monotonic.
        let (mut low, mut high) = (1, counts.iter().copied().max().unwrap_or(1));
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            if fits(&at(mid)) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let mut keep = at(low);
        for (i, &n) in counts.iter().enumerate() {
            if n > low
                && let Some(kept) = keep.get_mut(i)
            {
                *kept += 1;
                if !fits(&keep)
                    && let Some(kept) = keep.get_mut(i)
                {
                    *kept -= 1;
                }
            }
        }
        Some(build(&keep))
    }

    fn check(&self, query: &Query) -> Result<(), QueryError> {
        let mut usage = self.excluded;
        usage.expansions = usage.expansions.saturating_sub(self.credit(query));
        query.check(self.schema, &self.limits, &mut usage, 2)
    }
}

/// A `prefix` or `fuzzy` by what decides the terms it keeps (SPEC §7.1): its field, its bytes, its distance (0 for a
/// `prefix`) and its `max_expansions`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Expanded {
    fuzzy: bool,
    field: FieldId,
    bytes: String,
    distance: u8,
    cap: u32,
}

impl Expanded {
    /// Whether `other` keeps only terms this one keeps (SPEC §7.1): the same `prefix` with no more expansions, or a
    /// `fuzzy` of the same field and term at no greater distance with no more expansions, since a `fuzzy` keeps the
    /// fewest edits first, so the terms within a lower distance come first in the same order.
    fn subsumes(&self, other: &Self) -> bool {
        self.fuzzy == other.fuzzy
            && self.field == other.field
            && self.bytes == other.bytes
            && other.distance <= self.distance
            && other.cap <= self.cap
    }
}

/// Every `prefix` and `fuzzy` of `query` into `out`, in query order: those under a `must_not` too when `negated`.
fn expanded(query: &Query, negated: bool, out: &mut Vec<Expanded>) {
    match query {
        Query::Prefix(q) => out.push(Expanded {
            fuzzy: false,
            field: q.field,
            bytes: q.prefix.clone(),
            distance: 0,
            cap: q.max_expansions,
        }),
        Query::Fuzzy(q) => out.push(Expanded {
            fuzzy: true,
            field: q.field,
            bytes: q.term.clone(),
            distance: q.distance,
            cap: q.max_expansions,
        }),
        Query::Bool(q) => {
            for clause in q.must.iter().chain(&q.should) {
                expanded(clause, negated, out);
            }
            if negated {
                for clause in &q.must_not {
                    expanded(clause, negated, out);
                }
            }
        }
        Query::Boost(q) => expanded(&q.query, negated, out),
        Query::ConstantScore(q) => expanded(&q.query, negated, out),
        Query::Term(_) | Query::Combined(_) | Query::Phrase(_) | Query::MatchAll => {}
    }
}

/// The `max_expansions` of every `prefix` and `fuzzy` in `query` a cut may lower, in query order: not those under a
/// `must_not`.
fn caps_of<'q>(query: &'q mut Query, caps: &mut Vec<&'q mut u32>) {
    match query {
        Query::Prefix(q) => caps.push(&mut q.max_expansions),
        Query::Fuzzy(q) => caps.push(&mut q.max_expansions),
        // An exclusion's expansions count but are not cut: fewer would exclude less, so the step would match more than
        // its level built (SPEC §7.1.3).
        Query::Bool(q) => {
            for clause in q.must.iter_mut().chain(&mut q.should) {
                caps_of(clause, caps);
            }
        }
        Query::Boost(q) => caps_of(&mut q.query, caps),
        Query::ConstantScore(q) => caps_of(&mut q.query, caps),
        Query::Term(_) | Query::Combined(_) | Query::Phrase(_) | Query::MatchAll => {}
    }
}

/// Lowers `caps` until they sum to at most `room`, the largest first, none below 1 (each query keeps its most
/// frequent term, SPEC §7.1): every cap becomes `min(cap, level)` for the highest level that fits, and the room left
/// below the next level goes, one each, to the caps that level cut, in query order. `false` when even 1 each is too
/// many.
pub(super) fn share(caps: &mut [&mut u32], room: usize) -> bool {
    let sum = |level: u32| -> usize {
        caps.iter()
            .map(|c| usize::try_from((**c).min(level)).unwrap_or(usize::MAX))
            .fold(0usize, usize::saturating_add)
    };
    if sum(1) > room {
        return false;
    }
    let (mut low, mut high) = (1u32, caps.iter().map(|c| **c).max().unwrap_or(1));
    // The highest level whose sum fits: `low` fits, `high + 1` does not (or is past every cap).
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if sum(mid) <= room {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut left = room.saturating_sub(sum(low));
    for cap in caps.iter_mut() {
        let cut = **cap > low;
        **cap = (**cap).min(low);
        if cut && left > 0 {
            **cap += 1;
            left -= 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests: inputs are fixed.

    use super::*;
    use crate::query::{BoolQuery, BoostQuery, FuzzyQuery, PrefixQuery, TermQuery};
    use crate::schema::{FieldId, FieldKind, SchemaBuilder, TextOptions};

    fn schema() -> (CollectionSchema, FieldId) {
        let schema = SchemaBuilder::new("c")
            .field("body", FieldKind::Text(TextOptions::default()))
            .build()
            .unwrap();
        let body = schema.field("body").unwrap().id();
        (schema, body)
    }

    fn prefix(field: FieldId, max_expansions: u32) -> Query {
        prefix_of(field, "ru", max_expansions)
    }

    fn prefix_of(field: FieldId, prefix: &str, max_expansions: u32) -> Query {
        Query::Prefix(PrefixQuery {
            field,
            prefix: prefix.into(),
            max_expansions,
        })
    }

    fn fuzzy(field: FieldId, max_expansions: u32) -> Query {
        fuzzy_of(field, "shoe", max_expansions)
    }

    fn fuzzy_of(field: FieldId, term: &str, max_expansions: u32) -> Query {
        fuzzy_at(field, term, 1, max_expansions)
    }

    fn fuzzy_at(field: FieldId, term: &str, distance: u8, max_expansions: u32) -> Query {
        Query::Fuzzy(FuzzyQuery {
            field,
            term: term.into(),
            distance,
            max_expansions,
        })
    }

    fn terms(field: FieldId, n: usize) -> Query {
        Query::Bool(BoolQuery {
            should: (0..n)
                .map(|i| {
                    Query::Term(TermQuery {
                        field,
                        term: format!("t{i}"),
                    })
                })
                .collect(),
            ..BoolQuery::default()
        })
    }

    fn any(clauses: Vec<Query>) -> Query {
        Query::Bool(BoolQuery {
            should: clauses,
            ..BoolQuery::default()
        })
    }

    fn limits(max_terms: usize, max_expansions: usize) -> QueryLimits {
        QueryLimits {
            max_terms,
            max_expansions,
            ..QueryLimits::default()
        }
    }

    #[test]
    fn a_step_that_fits_beside_its_exclusions_is_searched_as_built() {
        let (schema, body) = schema();
        let fitting = StepLimits::new(&schema, limits(4, 100), &terms(body, 2)).unwrap();
        assert_eq!(fitting.fit(&terms(body, 2)), Ok(Fit::Fits));
        // The exclusions count: two terms of L0 and two of the step are all four.
        assert_eq!(fitting.fit(&terms(body, 3)), Ok(Fit::Skipped));
        let alone = StepLimits::new(&schema, limits(4, 100), &Query::MatchAll).unwrap();
        assert_eq!(alone.fit(&terms(body, 3)), Ok(Fit::Fits));
    }

    #[test]
    fn expansions_are_cut_to_what_the_exclusions_leave_the_largest_first() {
        let (schema, body) = schema();
        // L0 holds 50 of the 80 expansions: the step's 50 + 30 share the 30 left.
        let fitting =
            StepLimits::new(&schema, limits(100, 80), &prefix_of(body, "sh", 50)).unwrap();
        let step = any(vec![prefix(body, 50), fuzzy(body, 30)]);
        assert_eq!(
            fitting.fit(&step),
            Ok(Fit::Cut(any(vec![prefix(body, 15), fuzzy(body, 15)])))
        );
        let step = any(vec![prefix(body, 50), fuzzy(body, 4)]);
        assert_eq!(
            fitting.fit(&step),
            Ok(Fit::Cut(any(vec![prefix(body, 26), fuzzy(body, 4)])))
        );
        // A searched step is excluded from the later ones: 29 more, 1 left (of other bytes than the step's, or the step
        // would count it as its own, ROADMAP 8.2.12).
        let mut fitting = fitting;
        fitting.exclude(&prefix_of(body, "ro", 29));
        assert_eq!(
            fitting.fit(&prefix(body, 50)),
            Ok(Fit::Cut(prefix(body, 1)))
        );
        assert_eq!(fitting.fit(&step), Ok(Fit::Skipped));
    }

    /// An L1-shaped step: per run, `should: [the run, keep[i] alternatives]`, each a term.
    fn runs(field: FieldId, keep: &[usize]) -> Query {
        Query::Bool(BoolQuery {
            must: keep
                .iter()
                .map(|&k| {
                    let run =
                        std::iter::once("run".to_owned()).chain((0..k).map(|i| format!("t{i}")));
                    any(run
                        .map(|term| Query::Term(TermQuery { field, term }))
                        .collect())
                })
                .collect(),
            ..BoolQuery::default()
        })
    }

    #[test]
    fn synonym_alternatives_are_cut_the_longest_lists_first_keeping_their_first() {
        let (schema, body) = schema();
        // L0 is `match_all`, no term; the step has a term per run and one per alternative kept.
        let cut = |max_terms: usize, counts: &[usize]| {
            let fitting = StepLimits::new(&schema, limits(max_terms, 0), &Query::MatchAll).unwrap();
            fitting
                .cut_alternatives(counts, |keep| runs(body, keep))
                .map(|q| match q {
                    // Alternatives kept per run: its `should` less the run itself.
                    Query::Bool(b) => b
                        .must
                        .iter()
                        .map(|r| match r {
                            Query::Bool(r) => r.should.len() - 1,
                            _ => usize::MAX,
                        })
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
        };
        // 10 terms: 2 for the runs, 8 alternatives: the longer list to 5, the shorter kept whole.
        assert_eq!(cut(10, &[10, 3]), Some(vec![5, 3]));
        assert_eq!(cut(8, &[10, 3]), Some(vec![3, 3]));
        // 13 terms: 3 runs, 10 alternatives: every list at 3 is 9, and the one left goes to the first list cut.
        assert_eq!(cut(13, &[10, 10, 3]), Some(vec![4, 3, 3]));
        assert_eq!(cut(13, &[3, 10, 10]), Some(vec![3, 4, 3]));
        // The floor is one each: below it, the step is skipped.
        assert_eq!(cut(4, &[10, 10]), Some(vec![1, 1]));
        assert_eq!(cut(3, &[10, 10]), None);
        assert_eq!(cut(100, &[]), None);
        // Exclusions over the limits leave room for nothing.
        let full = StepLimits::new(&schema, limits(2, 0), &terms(body, 3)).unwrap();
        assert_eq!(full.cut_alternatives(&[1], |keep| runs(body, keep)), None);
        // Alternatives of different sizes: run 0's second is 5 terms, the others 1. At 8 terms one each fits (4), not
        // two each (10); run 0's second does not fit then, run 1's does. The ones kept are each run's first.
        let sized = |keep: &[usize]| {
            let sizes = [[1, 5], [1, 1]];
            Query::Bool(BoolQuery {
                must: (0..2)
                    .map(|run| {
                        let mut should = vec![Query::Term(TermQuery {
                            field: body,
                            term: format!("run{run}"),
                        })];
                        for (alt, &size) in sizes[run].iter().enumerate().take(keep[run]) {
                            should.push(any((0..size)
                                .map(|t| {
                                    Query::Term(TermQuery {
                                        field: body,
                                        term: format!("r{run}a{alt}t{t}"),
                                    })
                                })
                                .collect()));
                        }
                        any(should)
                    })
                    .collect(),
                ..BoolQuery::default()
            })
        };
        let fitting = StepLimits::new(&schema, limits(8, 0), &Query::MatchAll).unwrap();
        assert_eq!(
            fitting.cut_alternatives(&[2, 2], sized),
            Some(sized(&[1, 2]))
        );
        // Also when what overflowed does not add up (depth): L0 nests 4 deep under the root, the step 3.
        let deep = QueryLimits {
            max_depth: 4,
            ..QueryLimits::default()
        };
        let nested = any(vec![any(vec![terms(body, 1)])]);
        let full = StepLimits::new(&schema, deep, &nested).unwrap();
        assert_eq!(full.cut_alternatives(&[1], |keep| runs(body, keep)), None);
        let room = StepLimits::new(&schema, deep, &Query::MatchAll).unwrap();
        assert!(
            room.cut_alternatives(&[1], |keep| runs(body, keep))
                .is_some()
        );
    }

    #[test]
    fn a_step_no_cut_fits_is_skipped() {
        let (schema, body) = schema();
        // Too many terms: cutting expansions does not help.
        let fitting = StepLimits::new(&schema, limits(3, 1000), &Query::MatchAll).unwrap();
        let step = any(vec![prefix(body, 50), terms(body, 3)]);
        assert_eq!(fitting.fit(&step), Ok(Fit::Skipped));
        // Without a prefix or fuzzy there is nothing to cut.
        let fitting = StepLimits::new(&schema, limits(1000, 0), &Query::MatchAll).unwrap();
        assert_eq!(fitting.fit(&terms(body, 2)), Ok(Fit::Fits));
        assert_eq!(fitting.fit(&prefix(body, 1)), Ok(Fit::Skipped));
        // Too deep: a step sits two levels down, under `must`.
        let shallow = QueryLimits {
            max_depth: 2,
            ..QueryLimits::default()
        };
        let fitting = StepLimits::new(&schema, shallow, &Query::MatchAll).unwrap();
        assert_eq!(fitting.fit(&prefix(body, 5)), Ok(Fit::Fits));
        assert_eq!(fitting.fit(&any(vec![prefix(body, 5)])), Ok(Fit::Skipped));
        // L0 at the limit leaves room for a step without terms only; one past it (the caller's, held to other
        // limits), for no step at all.
        let exact = StepLimits::new(&schema, limits(2, 1000), &terms(body, 2)).unwrap();
        assert_eq!(exact.fit(&Query::MatchAll), Ok(Fit::Fits));
        assert_eq!(exact.fit(&terms(body, 1)), Ok(Fit::Skipped));
        let over = StepLimits::new(&schema, limits(2, 1000), &terms(body, 3)).unwrap();
        assert_eq!(over.fit(&Query::MatchAll), Ok(Fit::Skipped));
    }

    /// A step: `terms` terms and a prefix or fuzzy per cap, under one `should`, or alone when it is one leaf; beside
    /// them a `must_not` of a prefix per `excluded` cap when there is one, all of it under a `boost` when `boosted`. Its
    /// expansions are of bytes no other step `k` holds, so none counts once as another's (ROADMAP 8.2.12).
    fn step(
        field: FieldId,
        k: usize,
        (terms, caps, excluded, boosted): &(usize, Vec<u32>, Vec<u32>, bool),
    ) -> Query {
        let terms = *terms;
        let mut leaves: Vec<Query> = (0..terms)
            .map(|i| {
                Query::Term(TermQuery {
                    field,
                    term: format!("t{i}"),
                })
            })
            .collect();
        leaves.extend(caps.iter().enumerate().map(|(i, &c)| {
            let bytes = format!("x{k}_{i}");
            if i % 2 == 0 {
                prefix_of(field, &bytes, c)
            } else {
                fuzzy_of(field, &bytes, c)
            }
        }));
        let mut query = match leaves.len() {
            1 => leaves.pop().unwrap(),
            _ => any(leaves),
        };
        if !excluded.is_empty() {
            query = Query::Bool(BoolQuery {
                must: vec![query],
                must_not: excluded
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| prefix_of(field, &format!("n{k}_{i}"), c))
                    .collect(),
                ..BoolQuery::default()
            });
        }
        if *boosted {
            query = Query::Boost(BoostQuery {
                query: Box::new(query),
                boost: 2.0,
            });
        }
        query
    }

    fn caps(query: &Query) -> Vec<u32> {
        let mut query = query.clone();
        let mut caps = Vec::new();
        caps_of(&mut query, &mut caps);
        caps.into_iter().map(|c| *c).collect()
    }

    proptest::proptest! {
        /// SPEC §7.1.3: every step searched, as it runs (`bool { must: [step], must_not: [earlier] }`), is within the
        /// limits; a cut lowers caps only, keeps each at least 1 and uses all the room left; a skipped step fits no
        /// cut.
        #[test]
        fn every_step_searched_is_within_the_limits(
            steps in proptest::collection::vec(
                (
                    0_usize..4,
                    proptest::collection::vec(1_u32..100, 0..4),
                    proptest::collection::vec(1_u32..100, 0..2),
                    proptest::bool::ANY,
                ),
                1..6,
            ),
            max_terms in 1_usize..12,
            max_clauses in 1_usize..16,
            max_expansions in 0_usize..300,
            max_depth in 1_usize..5,
        ) {
            let (schema, body) = schema();
            let limits = QueryLimits { max_terms, max_clauses, max_expansions, max_depth, ..QueryLimits::default() };
            let queries: Vec<Query> = steps.iter().enumerate().map(|(k, s)| step(body, k, s)).collect();
            let mut fitting = StepLimits::new(&schema, limits, &queries[0]).unwrap();
            let mut searched = vec![queries[0].clone()];
            for query in &queries[1..] {
                let runs = |q: &Query| Query::Bool(BoolQuery {
                    must: vec![q.clone()],
                    must_not: searched.clone(),
                    ..BoolQuery::default()
                });
                match fitting.fit(query).unwrap() {
                    Fit::Fits => {
                        proptest::prop_assert_eq!(runs(query).validate(&schema, &limits), Ok(()));
                    }
                    Fit::Cut(cut) => {
                        proptest::prop_assert_eq!(runs(&cut).validate(&schema, &limits), Ok(()));
                        proptest::prop_assert!(runs(query).validate(&schema, &limits).is_err());
                        let (was, now) = (caps(query), caps(&cut));
                        proptest::prop_assert!(was.iter().zip(&now).all(|(w, n)| 1 <= *n && n <= w));
                        let mut used = Usage::default();
                        runs(&cut).check(&schema, &limits, &mut used, 1).unwrap();
                        proptest::prop_assert_eq!(used.expansions, max_expansions);
                        searched.push(cut.clone());
                        fitting.exclude(&cut);
                        continue;
                    }
                    Fit::Skipped => {
                        let mut floor = query.clone();
                        let mut all = Vec::new();
                        caps_of(&mut floor, &mut all);
                        for c in all {
                            *c = 1;
                        }
                        proptest::prop_assert!(runs(&floor).validate(&schema, &limits).is_err());
                        continue;
                    }
                }
                searched.push(query.clone());
                fitting.exclude(query);
            }
        }
    }

    /// ROADMAP 8.2.12: an excluded expansion that keeps only terms one of the step's keeps counts once: a `fuzzy` of the
    /// same field and term at no greater distance with no more expansions (a `fuzzy` keeps the fewest edits first), or
    /// the same `prefix` with no more.
    #[test]
    fn an_excluded_expansion_the_step_subsumes_counts_once() {
        let (schema, body) = schema();
        let near = fuzzy_at(body, "sandalls", 1, 50);
        let far = fuzzy_at(body, "sandalls", 2, 50);
        let mut fitting = StepLimits::new(&schema, limits(100, 60), &Query::MatchAll).unwrap();
        fitting.exclude(&near);
        // L4's `fuzzy` beside the `fuzzy:1` it excludes: 50, not 100.
        assert_eq!(fitting.fit(&far), Ok(Fit::Fits));
        // An identical one too, and one under a `bool` of the step.
        assert_eq!(fitting.fit(&near), Ok(Fit::Fits));
        assert_eq!(fitting.fit(&any(vec![far.clone()])), Ok(Fit::Fits));
        // Another term, a farther distance excluded than searched, more expansions excluded, or another kind: counted
        // again, so the step is cut to what the exclusion leaves.
        for (excluded, step, left) in [
            (fuzzy_at(body, "sandals", 1, 50), far.clone(), 10),
            (far.clone(), near.clone(), 10),
            (fuzzy_at(body, "sandalls", 1, 55), far.clone(), 5),
            (prefix_of(body, "sandalls", 50), far.clone(), 10),
        ] {
            let mut fitting = StepLimits::new(&schema, limits(100, 60), &Query::MatchAll).unwrap();
            fitting.exclude(&excluded);
            let mut cut = step.clone();
            if let Query::Fuzzy(f) = &mut cut {
                f.max_expansions = left;
            }
            assert_eq!(fitting.fit(&step), Ok(Fit::Cut(cut)), "{excluded:?}");
        }
        // A prefix: the same bytes with no more expansions.
        let mut fitting = StepLimits::new(&schema, limits(100, 60), &Query::MatchAll).unwrap();
        fitting.exclude(&prefix_of(body, "sa", 40));
        assert_eq!(fitting.fit(&prefix_of(body, "sa", 50)), Ok(Fit::Fits));
        assert_eq!(
            fitting.fit(&prefix_of(body, "san", 50)),
            Ok(Fit::Cut(prefix_of(body, "san", 20)))
        );
        // Under a `must_not` of the step, an expansion subsumes nothing: the step does not search it.
        let negated = Query::Bool(BoolQuery {
            must: vec![Query::MatchAll],
            must_not: vec![far.clone()],
            ..BoolQuery::default()
        });
        let mut fitting = StepLimits::new(&schema, limits(100, 60), &Query::MatchAll).unwrap();
        fitting.exclude(&near);
        assert_eq!(fitting.fit(&negated), Ok(Fit::Skipped));
        // Excluded expansions over the limit alone no longer skip every later step: one that subsumes them fits.
        let mut fitting = StepLimits::new(&schema, limits(100, 60), &Query::MatchAll).unwrap();
        fitting.exclude(&near);
        fitting.exclude(&near);
        assert_eq!(fitting.fit(&far), Ok(Fit::Fits));
        assert_eq!(fitting.fit(&Query::MatchAll), Ok(Fit::Skipped));
    }

    /// ROADMAP 8.2.12: a cut that lowers the subsuming expansion below the one it subsumes counts that one again, so
    /// the step is cut to what is left with it counted in full.
    #[test]
    fn a_cut_below_a_subsumed_expansion_counts_it_again() {
        let (schema, body) = schema();
        let mut fitting = StepLimits::new(&schema, limits(100, 40), &Query::MatchAll).unwrap();
        fitting.exclude(&fuzzy_at(body, "sandalls", 1, 30));
        // 50 is over 40; cut to 40 it still keeps the excluded 30's terms, which count once.
        assert_eq!(
            fitting.fit(&fuzzy_at(body, "sandalls", 2, 50)),
            Ok(Fit::Cut(fuzzy_at(body, "sandalls", 2, 40)))
        );
        // Beside a `prefix` of 15, the first cut (25 and 15) falls below the excluded 30, which then counts: the two
        // share the 10 left.
        let step = any(vec![
            fuzzy_at(body, "sandalls", 2, 50),
            prefix_of(body, "bo", 15),
        ]);
        assert_eq!(
            fitting.fit(&step),
            Ok(Fit::Cut(any(vec![
                fuzzy_at(body, "sandalls", 2, 5),
                prefix_of(body, "bo", 5),
            ])))
        );
    }

    #[test]
    fn a_steps_own_exclusions_count_but_are_not_cut() {
        let (schema, body) = schema();
        let fitting = StepLimits::new(&schema, limits(100, 30), &Query::MatchAll).unwrap();
        let with_not = |wanted: u32| {
            Query::Bool(BoolQuery {
                must: vec![prefix(body, wanted)],
                must_not: vec![fuzzy(body, 20)],
                ..BoolQuery::default()
            })
        };
        // The fuzzy exclusion keeps its 20; the prefix gets the 10 left.
        assert_eq!(fitting.fit(&with_not(50)), Ok(Fit::Cut(with_not(10))));
        // An exclusion alone over the limit cannot be cut: skipped.
        let fitting = StepLimits::new(&schema, limits(100, 15), &Query::MatchAll).unwrap();
        assert_eq!(fitting.fit(&with_not(1)), Ok(Fit::Skipped));
    }

    #[test]
    fn an_invalid_l0_is_an_error() {
        let (schema, body) = schema();
        let empty = Query::Prefix(PrefixQuery {
            field: body,
            prefix: String::new(),
            max_expansions: 5,
        });
        assert!(matches!(
            StepLimits::new(&schema, limits(1, 1), &empty),
            Err(QueryError::EmptyPrefix { .. })
        ));
    }

    #[test]
    fn an_invalid_step_is_an_error_not_a_skip() {
        let (schema, body) = schema();
        let fitting = StepLimits::new(&schema, limits(1000, 1000), &Query::MatchAll).unwrap();
        let empty = |max_expansions| {
            Query::Prefix(PrefixQuery {
                field: body,
                prefix: String::new(),
                max_expansions,
            })
        };
        assert!(matches!(
            fitting.fit(&empty(5)),
            Err(QueryError::EmptyPrefix { .. })
        ));
        // Also when it is over a limit: whether it is valid is checked before the limits.
        let fitting = StepLimits::new(&schema, limits(1000, 2), &Query::MatchAll).unwrap();
        assert!(matches!(
            fitting.fit(&empty(50)),
            Err(QueryError::EmptyPrefix { .. })
        ));
    }

    fn shared(caps: &[u32], room: usize) -> Option<Vec<u32>> {
        let mut caps = caps.to_vec();
        let mut refs: Vec<&mut u32> = caps.iter_mut().collect();
        share(&mut refs, room).then_some(caps)
    }

    #[test]
    fn share_cuts_the_largest_first_and_keeps_one_each() {
        assert_eq!(shared(&[50, 50], 100), Some(vec![50, 50]));
        assert_eq!(shared(&[50, 50], 99), Some(vec![50, 49]));
        assert_eq!(shared(&[50, 3, 20], 30), Some(vec![14, 3, 13]));
        assert_eq!(shared(&[50, 3, 20], 3), Some(vec![1, 1, 1]));
        assert_eq!(shared(&[50, 3, 20], 2), None);
        assert_eq!(shared(&[7], 1000), Some(vec![7]));
        assert_eq!(shared(&[u32::MAX, u32::MAX], 5), Some(vec![3, 2]));
    }
}

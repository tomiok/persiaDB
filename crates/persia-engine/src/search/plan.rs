//! A query planned against the collection statistics (SPEC §7.3): its scorers built once, then a matcher tree per
//! view.

use core::cell::Cell;
use core::ops::Bound;

use roaring::RoaringBitmap;

use crate::query::{
    BoolQuery, Filter, FuzzyQuery, PhraseQuery, PrefixQuery, Query, QueryError, QueryLimits,
};
use crate::schema::{CollectionSchema, Field, FieldId, FieldKind};
use crate::score::{FieldParams, ScoreError, TermScorer};
use crate::segment::{ReadError, prefix_successor};
use crate::stats::StatisticsProvider;
use crate::view::SearchView;

use super::block_max_wand::BlockMaxWand;
use super::boolean::{
    AllDocs, Conjunction, Disjunction, Empty, Exclusion, Optional, Scale, Scaled,
};
use super::check::{Check, Checked, check_share, checks, per_candidate};
use super::expand::{Expansion, TermSource, expand};
use super::explain::{Explanation, explain_all};
use super::filter::{FilterDocs, Filtered, Selections, narrow, select};
use super::max_score::MaxScore;
use super::phrase::{PhraseMatcher, PhrasePostings};
use super::word::{WordField, WordMatcher};
use super::{Hit, Matcher, Pruning, SearchError, collect};

/// A query ready to run over any view of the collection whose statistics planned it: a segment, the memtable
/// (SPEC §7.3).
#[derive(Debug, Clone)]
pub struct SearchPlan {
    root: Node,
    /// The share every `bool` checks its column filters per candidate at, for a benchmark ([`Self::with_check_share`]);
    /// `None`: the fitted ones ([`super::check::check_share`]).
    check_share: Option<u64>,
}

#[derive(Debug, Clone)]
pub(super) enum Node {
    /// A `term` or `combined`: per field of the scorer, its term and whether it is a `text` field (has norms).
    Word {
        terms: Vec<(FieldId, Vec<u8>, bool)>,
        scorer: TermScorer,
    },
    Phrase {
        field: FieldId,
        terms: Vec<(Vec<u8>, u32)>,
        scorer: TermScorer,
    },
    Bool {
        must: Vec<Node>,
        should: Vec<Node>,
        must_not: Vec<Node>,
        filter: Vec<Filter>,
        /// `filter[..split]` read postings, norms or ords and are selected as a bitmap; `filter[split..]` read only
        /// column keys, so a view may check them per candidate (ROADMAP 7.4.13, 7.4.16).
        split: usize,
        /// Unique in the plan: where a search keeps what its filters select ([`Selections`]); unused by a `bool`
        /// without filters (an expansion's).
        slot: usize,
        /// The fewest `should` clauses a match holds (SPEC §7.1); 0 for the default rule.
        minimum_should_match: usize,
    },
    Scaled(Box<Node>, Scale),
    All,
    /// No document: an expansion that kept no term.
    Nothing,
}

type BoxedMatcher<'a> = Box<dyn Matcher + 'a>;

impl SearchPlan {
    /// Plans `query` over a collection of `schema` whose statistics `stats` gives (SPEC §7.2). The query should
    /// have passed [`Query::validate`]: the limits are not checked again, and planning, like matching, recurses as
    /// deep as the query and its filters nest. What would plan into different results is rejected here too.
    ///
    /// # Errors
    /// [`SearchError::Score`] for a field a query does not apply to or a statistic that cannot be read;
    /// [`SearchError::Query`] for a phrase on a field without positions or with decreasing positions, and a boost
    /// or constant score out of range, and a filter that does not fit the schema ([`Filter::validate`], limits
    /// aside); [`SearchError::NeedsTerms`] for a `prefix` or a `fuzzy`, which only [`Self::with_terms`] plans.
    pub fn new(
        query: &Query,
        schema: &CollectionSchema,
        stats: &(impl StatisticsProvider + ?Sized),
    ) -> Result<Self, SearchError> {
        Ok(Self {
            check_share: None,
            root: plan(
                query,
                schema,
                &Planner {
                    stats,
                    terms: None,
                    slots: Cell::new(0),
                },
            )?,
        })
    }

    /// As [`Self::new`], and expands each `prefix` and `fuzzy` over the terms of `terms`, the views the plan will
    /// run on (SPEC §7.1): at most `max_expansions` terms each, the highest collection `df` (`stats`) first, then
    /// by bytes. Every view then scores the same terms, all with the largest of those `df`s. A `fuzzy` term is
    /// also multiplied by `(distance + 1 − d) / (distance + 1)`, `d` its edit distance. The prefix and the fuzzy
    /// term are raw bytes: the field analyzer is not run (a prefix `Run` does not match indexed `running`).
    ///
    /// ```
    /// use std::sync::Arc;
    /// use persia_analysis::AnalyzerRegistry;
    /// use persia_engine::document::{Document, Value};
    /// use persia_engine::query::{FuzzyQuery, PrefixQuery, Query};
    /// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    /// use persia_engine::search::SearchPlan;
    /// use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    /// use persia_engine::stats::SegmentStatistics;
    /// use persia_engine::view::SegmentView;
    /// use persia_format::ByteSource;
    ///
    /// let schema = SchemaBuilder::new("c").field("body", FieldKind::Text(TextOptions::default())).build()?;
    /// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
    /// let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
    /// for (id, text) in [("a", "running shoes"), ("b", "runner"), ("c", "red hat")] {
    ///     b.add(&Document::new(id).with(body, Value::String(text.into())))?;
    /// }
    /// let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
    /// let segment = Arc::new(Segment::open(ByteSource::new(SegmentWriter::new(&b, meta).finish()?.into()), &schema)?);
    /// let schema = Arc::new(schema);
    /// let stats = SegmentStatistics::new(&schema, &[Arc::clone(&segment)]);
    /// let view = SegmentView::new(&segment, None);
    ///
    /// let prefix = Query::Prefix(PrefixQuery { field: body, prefix: "run".into(), max_expansions: 50 });
    /// let hits = SearchPlan::with_terms(&prefix, &schema, &stats, &view)?.top_k(&view, 10)?;
    /// assert_eq!(hits.len(), 2);
    /// // `hut` is one substitution from `hat`.
    /// let fuzzy = Query::Fuzzy(FuzzyQuery { field: body, term: "hut".into(), distance: 1, max_expansions: 50 });
    /// let hits = SearchPlan::with_terms(&fuzzy, &schema, &stats, &view)?.top_k(&view, 10)?;
    /// assert_eq!(hits.iter().map(|h| h.docid).collect::<Vec<_>>(), [2]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Self::new`], but never [`SearchError::NeedsTerms`]; [`SearchError::Query`] for an empty prefix or a
    /// distance other than 1 or 2; [`SearchError::Read`] if `terms` cannot be read.
    pub fn with_terms(
        query: &Query,
        schema: &CollectionSchema,
        stats: &(impl StatisticsProvider + ?Sized),
        terms: &dyn TermSource,
    ) -> Result<Self, SearchError> {
        Ok(Self {
            check_share: None,
            root: plan(
                query,
                schema,
                &Planner {
                    stats,
                    terms: Some(terms),
                    slots: Cell::new(0),
                },
            )?,
        })
    }

    /// Benchmark hook (ROADMAP 7.4.16): this plan with its `bool`s checking their column filters per candidate when
    /// the candidates are at most `1 / share` of a view's documents (0: never), instead of the fitted shares. Results
    /// are the same at every share; only the cost moves.
    #[doc(hidden)]
    #[must_use]
    pub fn with_check_share(mut self, share: u64) -> Self {
        self.check_share = Some(share);
        self
    }

    fn build<'k>(&self, kept: Option<&'k Selections>) -> Build<'k> {
        Build {
            kept,
            check_share: self.check_share,
        }
    }

    /// The matcher of the plan over `view`: every match, scored ([`Pruning::Exhaustive`]).
    ///
    /// # Errors
    /// [`ReadError::UnknownField`] or [`ReadError::FieldType`] if `view` is not of the planned schema; a
    /// corruption of the view.
    pub fn matcher<'a, V: SearchView>(
        &'a self,
        view: &'a V,
    ) -> Result<BoxedMatcher<'a>, ReadError> {
        self.matcher_with(view, Pruning::Exhaustive)
    }

    /// The matcher of the plan over `view` that prunes with `pruning` once given a threshold
    /// ([`Matcher::set_min_competitive`]); until then it matches what [`Self::matcher`] does.
    ///
    /// # Errors
    /// As [`Self::matcher`].
    pub fn matcher_with<'a, V: SearchView>(
        &'a self,
        view: &'a V,
        pruning: Pruning,
    ) -> Result<BoxedMatcher<'a>, ReadError> {
        build(&self.root, view, pruning, self.build(None))
    }

    /// As [`Self::matcher_with`], keeping in `kept` what each `bool` selects with its filters, for
    /// [`Self::explain_kept`] over the same view (ROADMAP 7.7.3).
    pub(crate) fn matcher_keeping<'a, V: SearchView>(
        &'a self,
        view: &'a V,
        pruning: Pruning,
        kept: &Selections,
    ) -> Result<BoxedMatcher<'a>, ReadError> {
        build(&self.root, view, pruning, self.build(Some(kept)))
    }

    /// The best `k` hits of the plan in `view`: score descending, then docid ascending (ROADMAP 7.4.3), with the
    /// default [`Pruning`] (block-max WAND).
    ///
    /// # Errors
    /// As [`Self::matcher`].
    pub fn top_k<V: SearchView>(&self, view: &V, k: usize) -> Result<Vec<Hit>, ReadError> {
        self.top_k_with(view, k, Pruning::default())
    }

    /// The best `k` hits of the plan in `view`, pruning with `pruning`: the same hits whatever it is.
    ///
    /// # Errors
    /// As [`Self::matcher`].
    pub fn top_k_with<V: SearchView>(
        &self,
        view: &V,
        k: usize,
        pruning: Pruning,
    ) -> Result<Vec<Hit>, ReadError> {
        collect(&mut self.matcher_with(view, pruning)?, k)
    }

    /// How `docid` of `view` scores under the plan (SPEC §7.7): per word, its `idf`, `k1`, `tf~` and each field's
    /// frequency, norm and part of `tf~`; `None` when the plan does not match it (a deleted document included). The
    /// score is the one a search gives the hit, bit for bit. It costs about an unpruned search of `view`: explain
    /// the hits of a page in a view together with [`Self::explain_all`].
    ///
    /// ```
    /// use std::sync::Arc;
    /// use persia_analysis::AnalyzerRegistry;
    /// use persia_engine::document::{Document, Value};
    /// use persia_engine::query::{Query, TermQuery};
    /// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    /// use persia_engine::search::{Explanation, SearchPlan};
    /// use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    /// use persia_engine::stats::SegmentStatistics;
    /// use persia_engine::view::SegmentView;
    /// use persia_format::ByteSource;
    ///
    /// let schema = SchemaBuilder::new("c").field("body", FieldKind::Text(TextOptions::default())).build()?;
    /// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
    /// let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
    /// for (id, text) in [("a", "red shoes"), ("b", "blue hat")] {
    ///     b.add(&Document::new(id).with(body, Value::String(text.into())))?;
    /// }
    /// let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
    /// let segment = Arc::new(Segment::open(ByteSource::new(SegmentWriter::new(&b, meta).finish()?.into()), &schema)?);
    /// let schema = Arc::new(schema);
    /// let stats = SegmentStatistics::new(&schema, &[Arc::clone(&segment)]);
    /// let view = SegmentView::new(&segment, None);
    ///
    /// let plan = SearchPlan::new(&Query::Term(TermQuery { field: body, term: "red".into() }), &schema, &stats)?;
    /// let hit = plan.top_k(&view, 1)?[0];
    /// let Some(Explanation::Word(word)) = plan.explain(&view, hit.docid)? else { panic!("a word") };
    /// assert_eq!(word.score.to_bits(), hit.score.to_bits());
    /// assert_eq!((word.fields[0].tf, word.fields[0].length), (1, Some(2)));
    /// assert!(plan.explain(&view, 1)?.is_none()); // `blue hat` does not match
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Self::matcher`].
    pub fn explain<V: SearchView>(
        &self,
        view: &V,
        docid: u32,
    ) -> Result<Option<Explanation>, ReadError> {
        Ok(explain_all(&self.root, view, &[docid], None)?
            .pop()
            .flatten())
    }

    /// As [`Self::explain`] for each of `docids`, in that order, in one pass over `view` (ROADMAP 7.7.2): the tree is
    /// built once and walked in docid order, and each `bool` selects its filters once. Explaining the hits of a page
    /// in a view costs about one unpruned search of it, not one per hit.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use persia_analysis::AnalyzerRegistry;
    /// use persia_engine::document::{Document, Value};
    /// use persia_engine::query::{Query, TermQuery};
    /// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    /// use persia_engine::search::SearchPlan;
    /// use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    /// use persia_engine::stats::SegmentStatistics;
    /// use persia_engine::view::SegmentView;
    /// use persia_format::ByteSource;
    ///
    /// let schema = SchemaBuilder::new("c").field("body", FieldKind::Text(TextOptions::default())).build()?;
    /// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
    /// let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
    /// for (id, text) in [("a", "red shoes"), ("b", "blue hat"), ("c", "red red hat")] {
    ///     b.add(&Document::new(id).with(body, Value::String(text.into())))?;
    /// }
    /// let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
    /// let segment = Arc::new(Segment::open(ByteSource::new(SegmentWriter::new(&b, meta).finish()?.into()), &schema)?);
    /// let schema = Arc::new(schema);
    /// let stats = SegmentStatistics::new(&schema, &[Arc::clone(&segment)]);
    /// let view = SegmentView::new(&segment, None);
    ///
    /// let plan = SearchPlan::new(&Query::Term(TermQuery { field: body, term: "red".into() }), &schema, &stats)?;
    /// // Any order: the answers come back in the order asked.
    /// let explained = plan.explain_all(&view, &[2, 1, 0])?;
    /// assert_eq!(explained.iter().map(Option::is_some).collect::<Vec<_>>(), [true, false, true]);
    /// assert_eq!(explained[2], plan.explain(&view, 0)?);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Self::matcher`].
    pub fn explain_all<V: SearchView>(
        &self,
        view: &V,
        docids: &[u32],
    ) -> Result<Vec<Option<Explanation>>, ReadError> {
        explain_all(&self.root, view, docids, None)
    }

    /// As [`Self::explain_all`], each `bool` starting from what [`Self::matcher_keeping`] kept over the same view
    /// rather than selecting its filters again (ROADMAP 7.7.3). The selections are taken out of `kept`.
    pub(crate) fn explain_kept<V: SearchView>(
        &self,
        view: &V,
        docids: &[u32],
        kept: &Selections,
    ) -> Result<Vec<Option<Explanation>>, ReadError> {
        explain_all(&self.root, view, docids, Some(kept))
    }
}

/// What planning reads: the statistics, and the views' terms when it may expand.
struct Planner<'a, S: ?Sized> {
    stats: &'a S,
    terms: Option<&'a dyn TermSource>,
    /// The `bool`s planned so far: the next one's slot.
    slots: Cell<usize>,
}

impl<S: ?Sized> Planner<'_, S> {
    /// A slot no other `bool` of the plan has.
    fn slot(&self) -> usize {
        let slot = self.slots.get();
        self.slots.set(slot.saturating_add(1));
        slot
    }
}

fn plan<S: StatisticsProvider + ?Sized>(
    query: &Query,
    schema: &CollectionSchema,
    p: &Planner<'_, S>,
) -> Result<Node, SearchError> {
    let stats = p.stats;
    let is_text = |field| {
        matches!(
            schema.field_by_id(field).map(Field::kind),
            Some(FieldKind::Text(_))
        )
    };
    Ok(match query {
        Query::Term(q) => Node::Word {
            scorer: TermScorer::for_term(q, schema, stats)?,
            terms: vec![(q.field, q.term.as_bytes().to_vec(), is_text(q.field))],
        },
        Query::Combined(q) => Node::Word {
            scorer: TermScorer::for_combined(q, schema, stats)?,
            terms: q
                .fields
                .iter()
                .map(|c| (c.field, c.term.as_bytes().to_vec(), true))
                .collect(),
        },
        Query::Phrase(q) => {
            phrase_runs(q, schema)?;
            Node::Phrase {
                scorer: TermScorer::for_phrase(q, schema, stats)?,
                field: q.field,
                // Positions are relative to the first term, which need not be at 0.
                terms: q
                    .terms
                    .iter()
                    .map(|t| {
                        let first = q.terms.first().map_or(0, |f| f.position);
                        (t.term.as_bytes().to_vec(), t.position.saturating_sub(first))
                    })
                    .collect(),
            }
        }
        Query::Prefix(q) => plan_prefix(q, schema, p)?,
        Query::Fuzzy(q) => plan_fuzzy(q, schema, p)?,
        Query::Bool(q) => plan_bool(q, schema, p)?,
        Query::Boost(q) if !(q.boost.is_finite() && q.boost > 0.0 && q.boost <= 1000.0) => {
            return Err(QueryError::InvalidBoost { value: q.boost }.into());
        }
        Query::ConstantScore(q) if !(q.score.is_finite() && q.score >= 0.0) => {
            return Err(QueryError::InvalidScore { value: q.score }.into());
        }
        Query::Boost(q) => {
            Node::Scaled(Box::new(plan(&q.query, schema, p)?), Scale::Boost(q.boost))
        }
        Query::ConstantScore(q) => Node::Scaled(
            Box::new(plan(&q.query, schema, p)?),
            Scale::Constant(q.score),
        ),
        Query::MatchAll => Node::All,
    })
}

fn plan_bool<S: StatisticsProvider + ?Sized>(
    q: &BoolQuery,
    schema: &CollectionSchema,
    p: &Planner<'_, S>,
) -> Result<Node, SearchError> {
    // A value of another type than its field would select by the wrong keys. The limits are the caller's to check
    // (`Query::validate`), as for every other clause.
    for filter in &q.filter {
        filter.validate(schema, &UNLIMITED)?;
    }
    let all = |clauses: &[Query]| -> Result<Vec<Node>, SearchError> {
        clauses.iter().map(|c| plan(c, schema, p)).collect()
    };
    // The filters that read postings first, then those a view may check per candidate: a `bool`'s filters all
    // hold, so their order does not change what it selects. It changes what is read: a column after an empty
    // postings selection is not, so its corruption, if any, is not reported by that search.
    let (mut filter, column): (Vec<Filter>, Vec<Filter>) =
        q.filter.iter().cloned().partition(|f| !checks(f, schema));
    let split = filter.len();
    filter.extend(column);
    Ok(Node::Bool {
        must: all(&q.must)?,
        should: all(&q.should)?,
        must_not: all(&q.must_not)?,
        filter,
        split,
        // After the clauses: a `bool`'s slot follows its nested ones' (post-order).
        slot: p.slot(),
        minimum_should_match: usize::try_from(q.minimum_should_match).unwrap_or(usize::MAX),
    })
}

fn plan_prefix<S: StatisticsProvider + ?Sized>(
    q: &PrefixQuery,
    schema: &CollectionSchema,
    p: &Planner<'_, S>,
) -> Result<Node, SearchError> {
    let field = searchable(schema, q.field, "a prefix query")?;
    if q.prefix.is_empty() {
        return Err(QueryError::EmptyPrefix {
            field: field.name().to_owned(),
        }
        .into());
    }
    let start = q.prefix.as_bytes();
    let end = prefix_successor(start);
    let upper = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
    let range = (Bound::Included(start), upper);
    plan_expansion(
        schema,
        field,
        range,
        &Expansion::all(),
        q.max_expansions,
        ExpansionScore::Prefix,
        p,
    )
}

fn plan_fuzzy<S: StatisticsProvider + ?Sized>(
    q: &FuzzyQuery,
    schema: &CollectionSchema,
    p: &Planner<'_, S>,
) -> Result<Node, SearchError> {
    let field = searchable(schema, q.field, "a fuzzy query")?;
    if !matches!(q.distance, 1 | 2) {
        return Err(QueryError::InvalidDistance {
            distance: q.distance,
        }
        .into());
    }
    let expansion = Expansion::fuzzy(&q.term, q.distance);
    let range = (Bound::Unbounded, Bound::Unbounded);
    plan_expansion(
        schema,
        field,
        range,
        &expansion,
        q.max_expansions,
        ExpansionScore::Fuzzy {
            distance: q.distance,
        },
        p,
    )
}

/// The `text` or `keyword` field `field`, which a `prefix` or `fuzzy` takes (SPEC §7.1).
fn searchable<'s>(
    schema: &'s CollectionSchema,
    field: FieldId,
    what: &'static str,
) -> Result<&'s Field, SearchError> {
    match schema.field_by_id(field) {
        Some(f) if matches!(f.kind(), FieldKind::Text(_) | FieldKind::Keyword(_)) => Ok(f),
        _ => Err(ScoreError::Field { field, what }.into()),
    }
}

/// How a `prefix` or `fuzzy` scores the terms it kept (SPEC §7.1).
#[derive(Clone, Copy)]
enum ExpansionScore {
    /// One shared `idf`; each term weighs 1.
    Prefix,
    /// One shared `idf`, then `(distance + 1 − d) / (distance + 1)` for a term at edit distance `d`.
    Fuzzy { distance: u8 },
}

impl ExpansionScore {
    fn what(self) -> &'static str {
        match self {
            Self::Prefix => "prefix",
            Self::Fuzzy { .. } => "fuzzy",
        }
    }

    /// The multiplier of a kept `term` (SPEC §7.1). A prefix weighs 1. A fuzzy term farther than the query allows
    /// is not produced by the expansion; a missing distance weighs as the farthest accepted edit.
    fn weight(self, expansion: &Expansion, term: &[u8]) -> f64 {
        let Self::Fuzzy { distance } = self else {
            return 1.0;
        };
        let edits = expansion.distance(term).unwrap_or(distance).min(distance);
        edit_weight(distance, edits)
    }
}

/// `(max_distance + 1 − edits) / (max_distance + 1)` (SPEC §7.1). `edits` is at most `max_distance`, so the weight
/// is 1 for an exact term and at least `1 / (max_distance + 1)` for a kept edit.
fn edit_weight(max_distance: u8, edits: u8) -> f64 {
    let span = f64::from(max_distance) + 1.0;
    (span - f64::from(edits)) / span
}

/// A `prefix` or `fuzzy` as SPEC §7.1 plans it: the kept terms, in byte order, each a `term` with the largest of
/// their `df`s, a `fuzzy` term multiplied by its edit-distance weight. No document when none is kept. The bytes
/// are not analyzed.
fn plan_expansion<S: StatisticsProvider + ?Sized>(
    schema: &CollectionSchema,
    field: &Field,
    range: (Bound<&[u8]>, Bound<&[u8]>),
    expansion: &Expansion,
    max_expansions: u32,
    score: ExpansionScore,
    p: &Planner<'_, S>,
) -> Result<Node, SearchError> {
    let Some(terms) = p.terms else {
        return Err(SearchError::NeedsTerms { what: score.what() });
    };
    let cap = usize::try_from(max_expansions).unwrap_or(usize::MAX);
    let kept = expand(field.id(), range, expansion, cap, terms, p.stats)?;
    // The most common kept term. Every expansion then shares its `idf`, so a rarer one is not boosted by its own.
    let Some(blended) = kept.iter().map(|(_, df)| *df).max() else {
        return Ok(Node::Nothing);
    };
    let params = FieldParams::of(schema, field.id(), None)?;
    let text = matches!(field.kind(), FieldKind::Text(_));
    let mut words = Vec::with_capacity(kept.len());
    for (term, _) in kept {
        let word = Node::Word {
            scorer: TermScorer::new(&[params], blended, p.stats)?,
            terms: vec![(field.id(), term.clone(), text)],
        };
        let weight = score.weight(expansion, &term);
        // 1.0 is exact when the weight formula gives it (an exact fuzzy term, every prefix). Multiplying by it
        // would not be the same bits.
        words.push(if weight.to_bits() == 1.0_f64.to_bits() {
            word
        } else {
            Node::Scaled(Box::new(word), Scale::Boost(weight))
        });
    }
    Ok(match words.len() {
        0 => Node::Nothing,
        1 => words.pop().unwrap_or(Node::Nothing),
        _ => Node::Bool {
            must: Vec::new(),
            should: words,
            must_not: Vec::new(),
            filter: Vec::new(),
            split: 0,
            slot: p.slot(),
            minimum_should_match: 0,
        },
    })
}
/// Limits no filter reaches: planning checks a filter's fields and values, not its size.
const UNLIMITED: QueryLimits = QueryLimits::UNLIMITED;

/// The matcher of `node`. `pruning` is what a threshold on its score may skip with; the clauses a threshold on the
/// whole score says nothing about (a conjunction's, optional ones, exclusions, under a constant score) are built
/// [`Pruning::Exhaustive`]. Each `bool`'s selection is kept in `ctx.kept`, if given.
fn build<'a, V: SearchView>(
    node: &'a Node,
    view: &'a V,
    pruning: Pruning,
    ctx: Build<'_>,
) -> Result<BoxedMatcher<'a>, ReadError> {
    Ok(match node {
        Node::Word { terms, scorer } => match word_matcher(terms, scorer, view, pruning)? {
            Some(m) => Box::new(m),
            None => Box::new(Empty),
        },
        Node::Phrase {
            field,
            terms,
            scorer,
        } => match phrase_matcher(*field, terms, scorer, view)? {
            Some(m) => Box::new(m),
            None => Box::new(Empty),
        },
        Node::Bool {
            must,
            should,
            must_not,
            filter,
            split,
            slot,
            minimum_should_match,
        } => build_bool(
            Clauses {
                must,
                should,
                must_not,
                filter,
                split: *split,
                slot: *slot,
                minimum_should_match: *minimum_should_match,
            },
            view,
            pruning,
            ctx,
        )?,
        Node::Scaled(inner, scale) => {
            let inner_pruning = match scale {
                Scale::Boost(_) => pruning,
                Scale::Constant(_) => Pruning::Exhaustive,
            };
            Box::new(Scaled::new(build(inner, view, inner_pruning, ctx)?, *scale))
        }
        Node::All => Box::new(AllDocs::new(view)),
        Node::Nothing => Box::new(Empty),
    })
}

/// The matcher of a planned word over `view`; `None` when the view holds its term in none of its fields.
pub(super) fn word_matcher<'a, V: SearchView>(
    terms: &'a [(FieldId, Vec<u8>, bool)],
    scorer: &'a TermScorer,
    view: &'a V,
    pruning: Pruning,
) -> Result<Option<WordMatcher<'a, V>>, ReadError> {
    let mut fields = Vec::with_capacity(terms.len());
    for (slot, (field, term, text)) in terms.iter().enumerate() {
        let Some(postings) = view.postings(*field, term)? else {
            continue;
        };
        let norms = if *text {
            Some(view.norms(*field)?)
        } else {
            None
        };
        fields.push(WordField {
            field: *field,
            slot,
            postings,
            norms,
        });
    }
    Ok((!fields.is_empty()).then(|| WordMatcher::<V>::new(fields, scorer, pruning)))
}

/// The matcher of a planned phrase over `view`; `None` when the view lacks one of its terms.
pub(super) fn phrase_matcher<'a, V: SearchView>(
    field: FieldId,
    terms: &'a [(Vec<u8>, u32)],
    scorer: &'a TermScorer,
    view: &'a V,
) -> Result<Option<PhraseMatcher<'a, V>>, ReadError> {
    let mut postings = Vec::with_capacity(terms.len());
    for (term, position) in terms {
        let Some(p) = view.postings(field, term)? else {
            return Ok(None);
        };
        postings.push(PhrasePostings::new(p, *position));
    }
    if postings.is_empty() {
        return Ok(None);
    }
    Ok(Some(PhraseMatcher::<V>::new(
        postings,
        field,
        view.norms(field)?,
        scorer,
    )))
}

/// What building a plan's matcher over a view reads besides the plan's nodes.
#[derive(Clone, Copy)]
struct Build<'k> {
    /// Where each `bool` keeps its selection, if anywhere.
    kept: Option<&'k Selections>,
    /// As [`SearchPlan::check_share`].
    check_share: Option<u64>,
}

/// The clauses of a planned `bool`.
#[derive(Clone, Copy)]
struct Clauses<'a> {
    must: &'a [Node],
    should: &'a [Node],
    must_not: &'a [Node],
    filter: &'a [Filter],
    split: usize,
    slot: usize,
    minimum_should_match: usize,
}

/// A `bool` (SPEC §7.1): every `must` and `filter`, at least `minimum_should_match` `should` clauses (0: one when
/// there is neither `must` nor `filter` but some `should`), no `must_not`; an empty one matches every document.
/// Required `should` clauses are one disjunction of at least that many ([`at_least_of`]), alone or as the last
/// clause of the `must` conjunction. Filters select before anything is scored (SPEC §7.3) and score 0, so a
/// threshold on the `bool` reaches its one `must` clause, the `should` clauses when there is no `must` (their score
/// is the whole score), and otherwise the conjunction of its `must` clauses and the `should` clauses beside them,
/// which check their clauses' bounds against it (ROADMAP 7.4.8).
// Not inlined into the recursive `build`: since its filters were split (ROADMAP 7.4.16), inlining it made `build`
// large enough to slow requests that build no `bool`: a sorted `term` (`search_keyword_columns/sort_price` on
// rayon's pool) went 430 → 480 µs from 7.4.15 to 7.4.16, and back to about 444 µs with this (one-off A/B runs).
#[inline(never)]
fn build_bool<'a, V: SearchView>(
    clauses: Clauses<'a>,
    view: &'a V,
    pruning: Pruning,
    ctx: Build<'_>,
) -> Result<BoxedMatcher<'a>, ReadError> {
    let Clauses {
        must,
        should,
        must_not,
        filter,
        split,
        slot,
        minimum_should_match,
    } = clauses;
    let all = |nodes: &'a [Node], pruning| -> Result<Vec<BoxedMatcher<'a>>, ReadError> {
        nodes.iter().map(|n| build(n, view, pruning, ctx)).collect()
    };
    let filters = Filters {
        filter,
        split,
        slot,
    };
    // The score is the `should` clauses' when they are required, or the one `must` clause's without `should`.
    // No `must`: a filter adds 0, so the threshold reaches `should` whether or not a filter is present.
    // One `must` and no `should`: the threshold reaches that clause. Several `must`, or `should` beside `must`: the
    // conjunction and the optional matcher take it and check their clauses' bounds against it, the clauses get none
    // (what one may skip depends on what the others score; ROADMAP 7.4.8).
    let (must_pruning, should_pruning) = match (must.len(), should.is_empty()) {
        (0, _) => (Pruning::Exhaustive, pruning),
        (1, true) => (pruning, Pruning::Exhaustive),
        _ => (Pruning::Exhaustive, Pruning::Exhaustive),
    };
    // How many `should` clauses a match needs (SPEC §7.1): `minimum_should_match`, else one when nothing else is
    // required, else none (they are optional).
    let at_least = match minimum_should_match {
        0 if must.is_empty() && filter.is_empty() && !should.is_empty() => 1,
        m => m,
    };
    let scored: BoxedMatcher<'a> = if at_least > 0 {
        // Required `should` clauses: fewer than `at_least` that can match in this view means no document. The
        // other clauses are built anyway, so that a view they do not fit is the error it is for any count.
        let Some(some) = at_least_of(all(should, should_pruning)?, at_least, should_pruning) else {
            all(must, must_pruning)?;
            all(must_not, Pruning::Exhaustive)?;
            return Ok(Box::new(Empty));
        };
        let required = if must.is_empty() {
            some
        } else {
            // The `must` clauses, then the `should` ones as one more clause: summed in query order, as
            // `Optional` adds the two sides, so the score is the same bits.
            let mut clauses = all(must, must_pruning)?;
            clauses.push(some);
            conjunction(clauses, pruning)
        };
        filtered(required, filters, view, ctx)?
    } else {
        let optional = disjunction(all(should, should_pruning)?, should_pruning);
        let required: BoxedMatcher<'a> = if !must.is_empty() {
            filtered(
                conjunction(all(must, must_pruning)?, pruning),
                filters,
                view,
                ctx,
            )?
        } else if !filter.is_empty() {
            let docs = select(filter, view)?;
            keep(ctx, slot, &docs, false);
            if docs.is_empty() {
                Box::new(Empty)
            } else {
                Box::new(FilterDocs::new(docs))
            }
        } else {
            // No `should` either (`at_least` would be 1): every document.
            Box::new(AllDocs::new(view))
        };
        match optional {
            Some(optional) => Box::new(Optional::new(required, optional, pruning)),
            None => required,
        }
    };
    let excluded = disjunction(all(must_not, Pruning::Exhaustive)?, Pruning::Exhaustive);
    Ok(match excluded {
        Some(excluded) => Box::new(Exclusion::new(scored, excluded)),
        None => scored,
    })
}

/// A `bool`'s filters as [`Node::Bool`] orders them, and its slot.
#[derive(Clone, Copy)]
struct Filters<'a> {
    filter: &'a [Filter],
    split: usize,
    slot: usize,
}

/// The documents of `required` that every filter selects (SPEC §7.3). The filters that read postings
/// (`filter[..split]`) are selected as a bitmap; when `required` matches few enough documents
/// ([`per_candidate`]), the column filters (`filter[split..]`) are checked per candidate (ROADMAP 7.4.13, 7.4.16),
/// else selected too. The filters are not read when there are none, or when `required` matches nothing in the view.
fn filtered<'a, V: SearchView>(
    required: BoxedMatcher<'a>,
    filters: Filters<'a>,
    view: &'a V,
    ctx: Build<'_>,
) -> Result<BoxedMatcher<'a>, ReadError> {
    let Filters {
        filter,
        split,
        slot,
    } = filters;
    if filter.is_empty() || required.cost() == 0 {
        return Ok(required);
    }
    let (postings, column) = filter.split_at(split.min(filter.len()));
    // The `must` clauses' cost decides, not the candidates the postings filters leave: the clauses still walk their
    // matches among those documents, so a check pays off below a lower share beside them ([`check_share`]).
    // The sweep that fit both is ROADMAP 7.4.16's (docs/benchmarks.md).
    let candidates = required.cost();
    let mut docs = None;
    if !postings.is_empty() {
        let selected = select(postings, view)?;
        if selected.is_empty() {
            keep(ctx, slot, &selected, false);
            return Ok(Box::new(Empty));
        }
        docs = Some(selected);
    }
    let share = ctx
        .check_share
        .unwrap_or_else(|| check_share(!postings.is_empty()));
    let check = !column.is_empty() && per_candidate(candidates, view.doc_count(), share);
    if check {
        let check = Check::new(column, view)?;
        return Ok(match docs {
            Some(docs) => {
                keep(ctx, slot, &docs, true);
                Box::new(Checked::new(Filtered::new(required, docs), check))
            }
            // Nothing kept: an explanation checks the column filters as this does.
            None => Box::new(Checked::new(required, check)),
        });
    }
    let docs = match docs {
        Some(mut docs) => {
            narrow(&mut docs, column, view)?;
            docs
        }
        None => select(column, view)?,
    };
    keep(ctx, slot, &docs, false);
    Ok(if docs.is_empty() {
        Box::new(Empty)
    } else {
        Box::new(Filtered::new(required, docs))
    })
}

/// Keeps `docs`, what the `bool` at `slot` selected (of its postings filters only when `checked`), if `ctx` keeps.
fn keep(ctx: Build<'_>, slot: usize, docs: &RoaringBitmap, checked: bool) {
    if let Some(kept) = ctx.kept {
        kept.keep(slot, docs, checked);
    }
}

/// AND of at least one clause; no document if a clause has none in the view.
fn conjunction(mut clauses: Vec<BoxedMatcher<'_>>, pruning: Pruning) -> BoxedMatcher<'_> {
    if clauses.iter().any(|c| c.cost() == 0) {
        return Box::new(Empty);
    }
    if clauses.len() == 1
        && let Some(only) = clauses.pop()
    {
        return only;
    }
    Box::new(Conjunction::new(clauses, pruning))
}

/// OR of the clauses that can match, pruning with `pruning`; `None` when none can.
fn disjunction(mut clauses: Vec<BoxedMatcher<'_>>, pruning: Pruning) -> Option<BoxedMatcher<'_>> {
    clauses.retain(|c| c.cost() > 0);
    match clauses.len() {
        0 => None,
        1 => clauses.pop(),
        _ => Some(match pruning {
            Pruning::Exhaustive => Box::new(Disjunction::new(clauses)),
            Pruning::MaxScore => Box::new(MaxScore::new(clauses)),
            Pruning::BlockMaxWand => Box::new(BlockMaxWand::new(clauses)),
        }),
    }
}

/// The documents at least `at_least` (≥ 1) of the clauses match, pruning with `pruning`, scored as their OR;
/// `None` when fewer clauses than that can match in the view. All of them required is their AND: the same
/// documents and, the AND summing in query order as the OR does, the same score bits.
fn at_least_of(
    mut clauses: Vec<BoxedMatcher<'_>>,
    at_least: usize,
    pruning: Pruning,
) -> Option<BoxedMatcher<'_>> {
    clauses.retain(|c| c.cost() > 0);
    if clauses.len() < at_least {
        return None;
    }
    if at_least <= 1 {
        return disjunction(clauses, pruning);
    }
    if clauses.len() == at_least {
        return Some(Box::new(Conjunction::new(clauses, pruning)));
    }
    Some(match pruning {
        Pruning::Exhaustive => Box::new(Disjunction::at_least(clauses, at_least)),
        Pruning::MaxScore => Box::new(MaxScore::at_least(clauses, at_least)),
        Pruning::BlockMaxWand => Box::new(BlockMaxWand::at_least(clauses, at_least)),
    })
}

/// Checks what would otherwise silently change what a phrase matches (SPEC §7.1): its field indexes positions,
/// and its terms are in non-decreasing positions.
fn phrase_runs(q: &PhraseQuery, schema: &CollectionSchema) -> Result<(), QueryError> {
    let Some(field) = schema.field_by_id(q.field) else {
        return Err(QueryError::UnknownFieldId { field: q.field });
    };
    if let FieldKind::Text(o) = field.kind()
        && !o.positions
    {
        return Err(QueryError::NoPositions {
            field: field.name().to_owned(),
        });
    }
    if q.terms.windows(2).any(|w| match w {
        [a, b] => a.position > b.position,
        _ => false,
    }) {
        return Err(QueryError::InvalidPhrase {
            field: field.name().to_owned(),
        });
    }
    Ok(())
}

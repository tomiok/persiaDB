//! Filters checked one candidate at a time (SPEC §7.3's "column predicates", ROADMAP 7.4.13). A `bool` whose
//! `must` clauses match few documents of a view reads each candidate's column keys instead of scanning the whole
//! column into a bitmap ([`super::filter`]). Its documents and scores are the same either way.
//!
//! A filter can be checked this way when it reads only column keys: a `range`, and `term`, `terms` and `exists`
//! on an `i64`, `f64`, `datetime` or `bool` field, and `and`, `or` and `not` of those. A `keyword` value reads
//! postings, and `exists` on a `text` or `keyword` field reads norms or ords: a `bool` selects those as a bitmap,
//! then checks the others per candidate among the documents left (ROADMAP 7.4.16). A candidate comes from a matcher,
//! which yields live documents only, so the deleted ones are left out already; `not` is then the complement among
//! them.

use super::filter::{among, between, key, key_range};
use super::{Matcher, Pos, ScoreBound};
use crate::query::{Filter, FilterValue};
use crate::schema::{CollectionSchema, FieldType};
use crate::segment::{Overlap, ReadError};
use crate::view::{KeyReader, SearchView};

/// A `bool`'s column filters are checked per candidate rather than scanned into a bitmap when its `must` clauses
/// match at most `1 / CHECK_SHARE` of the view's documents: a check reads a key per candidate, a scan one per
/// document (bar the FOR blocks its bounds settle) and builds a bitmap, which then lets the clauses skip what it does
/// not hold. Fitted on `search_top10_filtered_sweep` (ROADMAP 7.4.16, docs/benchmarks.md): with column filters alone a
/// check wins up to about 35 % of the view (and loses at 50 %).
const CHECK_SHARE: u64 = 3;

/// As [`CHECK_SHARE`], beside filters that read postings (a `keyword` value): their bitmap is selected either way, and
/// a check leaves the clauses walking every candidate in it rather than in the smaller bitmap of all the filters. On the
/// same sweep a check wins for a single word up to 17 % (the most common word; none costs more) and for an OR up to
/// about 12 %, and loses for an OR from 18 %: a fifth keeps the single words and costs the ORs at 18–20 % a few
/// percent.
const CHECK_SHARE_BESIDE_POSTINGS: u64 = 5;

/// The share a `bool` checks its column filters at: [`CHECK_SHARE_BESIDE_POSTINGS`] when it has filters that read
/// postings, else [`CHECK_SHARE`].
pub(super) fn check_share(beside_postings: bool) -> u64 {
    if beside_postings {
        CHECK_SHARE_BESIDE_POSTINGS
    } else {
        CHECK_SHARE
    }
}

/// Whether every filter of `filters` reads only column keys, so it can be checked per candidate.
pub(super) fn checkable(filters: &[Filter], schema: &CollectionSchema) -> bool {
    filters.iter().all(|f| checks(f, schema))
}

/// Whether `filter` reads only column keys, so it can be checked per candidate.
pub(super) fn checks(filter: &Filter, schema: &CollectionSchema) -> bool {
    let column = |field| {
        schema.field_by_id(field).is_some_and(|f| {
            matches!(
                f.field_type(),
                FieldType::I64 | FieldType::F64 | FieldType::Datetime | FieldType::Bool
            )
        })
    };
    match filter {
        Filter::Term { field, value } => column(*field) && key(value).is_some(),
        Filter::Terms { field, values } => {
            column(*field) && values.iter().all(|v| key(v).is_some())
        }
        Filter::Range { field, .. } | Filter::Exists { field } => column(*field),
        Filter::And(filters) | Filter::Or(filters) => checkable(filters, schema),
        Filter::Not(filter) => checks(filter, schema),
    }
}

/// Whether `cost` candidates over a view of `doc_count` documents should be checked one at a time: `cost * share <=
/// doc_count` ([`check_share`]). Share 0 never checks; share 1 checks whenever `cost` is at most `doc_count`, which an
/// OR's cost (the sum of its clauses') can pass.
pub(super) fn per_candidate(cost: u64, doc_count: u32, share: u64) -> bool {
    share != 0 && cost.saturating_mul(share) <= u64::from(doc_count)
}

/// What a column's keys must be for a document to pass.
#[derive(Debug, Clone)]
enum Accept {
    /// Within `low..=high`.
    Range(u64, u64),
    /// One of these, sorted and distinct.
    Among(Vec<u64>),
    /// Any: the document has a value.
    Any,
    /// None: an empty range or list.
    Nothing,
}

impl Accept {
    fn overlap(&self, min: u64, max: u64) -> Overlap {
        match self {
            Self::Range(low, high) => between(*low, *high, min, max),
            Self::Among(keys) => among(keys, min, max),
            Self::Any => Overlap::Partial,
            Self::Nothing => Overlap::Disjoint,
        }
    }

    fn accepts(&self, key: u64) -> bool {
        match self {
            Self::Range(low, high) => (*low..=*high).contains(&key),
            Self::Among(keys) => keys.binary_search(&key).is_ok(),
            Self::Any => true,
            Self::Nothing => false,
        }
    }
}

/// A filter over one view's column readers.
enum Test<K> {
    Keys { keys: K, accept: Accept },
    And(Vec<Test<K>>),
    Or(Vec<Test<K>>),
    Not(Box<Test<K>>),
}

impl<K: KeyReader> Test<K> {
    fn new<'a, V: SearchView<Keys<'a> = K> + 'a>(
        filter: &Filter,
        view: &'a V,
    ) -> Result<Self, ReadError> {
        let keys = |field, accept| -> Result<Self, ReadError> {
            Ok(Self::Keys {
                keys: view.keys(field)?,
                accept,
            })
        };
        Ok(match filter {
            Filter::Term { field, value } => {
                keys(*field, among_values(core::slice::from_ref(value)))?
            }
            Filter::Terms { field, values } => keys(*field, among_values(values))?,
            Filter::Range {
                field,
                lower,
                upper,
            } => keys(
                *field,
                key_range(lower, upper).map_or(Accept::Nothing, |(l, h)| Accept::Range(l, h)),
            )?,
            Filter::Exists { field } => keys(*field, Accept::Any)?,
            Filter::And(filters) => Self::And(all(filters, view)?),
            Filter::Or(filters) => Self::Or(all(filters, view)?),
            Filter::Not(filter) => Self::Not(Box::new(Self::new(filter, view)?)),
        })
    }

    /// Whether `docid` passes. Docids ascend from one call to the next, the readers' fast path.
    fn passes(&mut self, docid: u32) -> Result<bool, ReadError> {
        Ok(match self {
            Self::Keys { keys, accept } => {
                // A block whose keys all fail is not decoded: the document fails, with a value or without.
                let disjoint = keys
                    .block(docid)?
                    .is_some_and(|b| accept.overlap(b.min, b.max) == Overlap::Disjoint);
                !disjoint && keys.key(docid)?.is_some_and(|k| accept.accepts(k))
            }
            Self::And(tests) => {
                for t in tests {
                    if !t.passes(docid)? {
                        return Ok(false);
                    }
                }
                true
            }
            Self::Or(tests) => {
                for t in tests {
                    if t.passes(docid)? {
                        return Ok(true);
                    }
                }
                false
            }
            Self::Not(test) => !test.passes(docid)?,
        })
    }
}

fn all<'a, V: SearchView + 'a>(
    filters: &[Filter],
    view: &'a V,
) -> Result<Vec<Test<V::Keys<'a>>>, ReadError> {
    filters.iter().map(|f| Test::new(f, view)).collect()
}

/// The keys of `values`; a `keyword` value has none (and is not [`checkable`]).
fn among_values(values: &[FilterValue]) -> Accept {
    let mut keys: Vec<u64> = values.iter().filter_map(key).collect();
    keys.sort_unstable();
    keys.dedup();
    if keys.is_empty() {
        Accept::Nothing
    } else {
        Accept::Among(keys)
    }
}

/// A `bool`'s filters over one view, checked one document at a time: they must be [`checkable`].
pub(super) struct Check<K>(Vec<Test<K>>);

impl<K: KeyReader> Check<K> {
    /// The filters `filters` over `view`'s column readers.
    ///
    /// # Errors
    /// A filter on a field `view` does not have with that type (a plan of another schema); a corruption of the view.
    pub(super) fn new<'a, V: SearchView<Keys<'a> = K> + 'a>(
        filters: &[Filter],
        view: &'a V,
    ) -> Result<Self, ReadError> {
        Ok(Self(all(filters, view)?))
    }

    /// Whether every filter selects `docid`, a live document. Docids ascend from one call to the next.
    ///
    /// # Errors
    /// A corruption of a column read.
    pub(super) fn passes(&mut self, docid: u32) -> Result<bool, ReadError> {
        for t in &mut self.0 {
            if !t.passes(docid)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The documents of `inner` its filters pass, checked one at a time and scored by `inner` (a filter adds 0).
pub(super) struct Checked<M, K> {
    inner: M,
    check: Check<K>,
    pos: Pos,
}

impl<M: Matcher, K: KeyReader> Checked<M, K> {
    pub(super) fn new(inner: M, check: Check<K>) -> Self {
        Self {
            inner,
            check,
            pos: Pos::Unstarted,
        }
    }

    /// From `doc`, the inner matcher's current document, to the first that passes.
    fn settle(&mut self, mut doc: Option<u32>) -> Result<Option<u32>, ReadError> {
        while let Some(d) = doc {
            if self.check.passes(d)? {
                break;
            }
            doc = self.inner.next()?;
        }
        self.pos = Pos::of(doc);
        Ok(doc)
    }
}

impl<M: Matcher, K: KeyReader> Matcher for Checked<M, K> {
    fn cost(&self) -> u64 {
        self.inner.cost()
    }

    fn doc(&self) -> Option<u32> {
        self.pos.doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        if self.pos == Pos::Done {
            return Ok(None);
        }
        let doc = self.inner.next()?;
        self.settle(doc)
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        match self.pos {
            Pos::At(d) if d >= target => Ok(Some(d)),
            Pos::Done => Ok(None),
            Pos::Unstarted | Pos::At(_) => {
                let doc = self.inner.advance(target)?;
                self.settle(doc)
            }
        }
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        if self.pos.doc().is_none() {
            return Ok(0.0);
        }
        self.inner.score()
    }

    fn max_score(&self) -> f64 {
        self.inner.max_score()
    }

    fn block_bound(&mut self, target: u32) -> Result<Option<ScoreBound>, ReadError> {
        self.inner.block_bound(target)
    }

    fn set_min_competitive(&mut self, score: f64) {
        self.inner.set_min_competitive(score);
    }
}

#[cfg(test)]
mod tests {
    use super::{check_share, per_candidate};

    /// The shares fitted on `search_top10_filtered_sweep` (ROADMAP 7.4.16, docs/benchmarks.md): a third of a view
    /// with column filters alone, a fifth beside filters that read postings.
    #[test]
    fn the_fitted_shares() {
        assert_eq!((check_share(false), check_share(true)), (3, 5));
        assert!(per_candidate(333, 1_000, check_share(false)));
        assert!(!per_candidate(334, 1_000, check_share(false)));
        assert!(per_candidate(200, 1_000, check_share(true)));
        assert!(!per_candidate(201, 1_000, check_share(true)));
        // 1 always checks, 0 never.
        assert!(per_candidate(1_000, 1_000, 1));
        assert!(!per_candidate(0, 1_000, 0));
    }
}

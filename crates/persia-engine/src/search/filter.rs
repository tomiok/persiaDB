//! The `filter` clauses of a `bool` (SPEC §7.1.2, §7.3, ROADMAP 7.4.5): a view evaluates them to a bitmap of the live
//! documents they select before any document is scored. [`FilterDocs`] matches that bitmap, scoring 0; [`Filtered`]
//! matches a matcher's documents in it with the matcher's scores and bounds, so a threshold reaches what it wraps.
//!
//! A `term` or `terms` filter on a `keyword` field reads the field's postings; on another field, and a `range`, it
//! scans the column's order-preserving keys (SPEC §5.2.1); `exists` reads which documents have a value. `not` takes
//! the complement among the view's documents, and deleted ones are taken out once, at the end.

use core::ops::Bound;

use persia_format::for_codec::{f64_to_key, i64_to_key};
use roaring::RoaringBitmap;
use roaring::bitmap::IntoIter;

use super::{Matcher, Pos, ScoreBound};
use crate::query::{Filter, FilterValue};
use crate::schema::FieldId;
use crate::segment::ReadError;
use crate::view::{PostingsIterator, SearchView};

/// The live documents of `view` that every filter of `filters` selects (none: every live document).
///
/// # Errors
/// A filter on a field `view` does not have with that type (a plan of another schema); a corruption of the view.
pub(super) fn select<V: SearchView>(
    filters: &[Filter],
    view: &V,
) -> Result<RoaringBitmap, ReadError> {
    let mut docs = all(filters, view)?;
    docs -= view.deleted();
    Ok(docs)
}

/// The documents every filter selects, deleted ones included.
fn all<V: SearchView>(filters: &[Filter], view: &V) -> Result<RoaringBitmap, ReadError> {
    let mut docs = every_doc(view);
    for filter in filters {
        if docs.is_empty() {
            break;
        }
        docs &= eval(filter, view)?;
    }
    Ok(docs)
}

fn every_doc<V: SearchView>(view: &V) -> RoaringBitmap {
    let mut docs = RoaringBitmap::new();
    docs.insert_range(0..view.doc_count());
    docs
}

/// The documents `filter` selects, deleted ones included.
fn eval<V: SearchView>(filter: &Filter, view: &V) -> Result<RoaringBitmap, ReadError> {
    match filter {
        Filter::Term { field, value } => holding(view, *field, core::slice::from_ref(value)),
        Filter::Terms { field, values } => holding(view, *field, values),
        Filter::Range {
            field,
            lower,
            upper,
        } => within(view, *field, lower, upper),
        Filter::Exists { field } => view.present(*field),
        Filter::And(filters) => all(filters, view),
        Filter::Or(filters) => {
            let mut docs = RoaringBitmap::new();
            for filter in filters {
                docs |= eval(filter, view)?;
            }
            Ok(docs)
        }
        Filter::Not(filter) => Ok(every_doc(view) - eval(filter, view)?),
    }
}

/// The documents whose `field` holds any of `values`.
fn holding<V: SearchView>(
    view: &V,
    field: FieldId,
    values: &[FilterValue],
) -> Result<RoaringBitmap, ReadError> {
    let mut docs = RoaringBitmap::new();
    let mut keys = Vec::new();
    for value in values {
        match value {
            FilterValue::Keyword(term) => {
                if let Some(mut postings) = view.postings(field, term.as_bytes())? {
                    let mut list = Vec::new();
                    while let Some(docid) = postings.next()? {
                        list.push(docid);
                    }
                    docs |= sorted(list);
                }
            }
            other => keys.extend(key(other)),
        }
    }
    if !keys.is_empty() {
        keys.sort_unstable();
        keys.dedup();
        docs |= scan(view, field, |k| keys.binary_search(&k).is_ok())?;
    }
    Ok(docs)
}

/// The documents whose `field` holds a value within the bounds, compared as keys: key order is value order.
fn within<V: SearchView>(
    view: &V,
    field: FieldId,
    lower: &Bound<FilterValue>,
    upper: &Bound<FilterValue>,
) -> Result<RoaringBitmap, ReadError> {
    // An exclusive bound is the inclusive one next to it; none past the end of the key space means no document.
    let low = match lower {
        Bound::Included(v) => key(v),
        Bound::Excluded(v) => key(v).and_then(|k| k.checked_add(1)),
        Bound::Unbounded => Some(0),
    };
    let high = match upper {
        Bound::Included(v) => key(v),
        Bound::Excluded(v) => key(v).and_then(|k| k.checked_sub(1)),
        Bound::Unbounded => Some(u64::MAX),
    };
    match (low, high) {
        (Some(low), Some(high)) if low <= high => scan(view, field, |k| (low..=high).contains(&k)),
        _ => Ok(RoaringBitmap::new()),
    }
}

/// The documents whose key in `field` satisfies `keep`.
fn scan<V: SearchView>(
    view: &V,
    field: FieldId,
    keep: impl Fn(u64) -> bool,
) -> Result<RoaringBitmap, ReadError> {
    let mut docids = Vec::new();
    view.column_keys(field, |docid, k| {
        if keep(k) {
            docids.push(docid);
        }
    })?;
    Ok(sorted(docids))
}

/// `docids` as a bitmap, built in bulk: postings and column scans give them in increasing order, which is many times
/// faster to build from than inserting each.
fn sorted(docids: Vec<u32>) -> RoaringBitmap {
    RoaringBitmap::from_sorted_iter(docids.iter().copied())
        .unwrap_or_else(|_| docids.into_iter().collect())
}

/// The column key of `value` (SPEC §5.2.1), as the builder stores it: `-0.0` is `0.0` (SPEC §6.4.1); `None` for a
/// `keyword`, which has postings instead.
fn key(value: &FilterValue) -> Option<u64> {
    Some(match *value {
        FilterValue::I64(v) | FilterValue::Datetime(v) => i64_to_key(v),
        FilterValue::F64(v) => f64_to_key(if v == 0.0 { 0.0 } else { v }),
        FilterValue::Bool(v) => u64::from(v),
        FilterValue::Keyword(_) => return None,
    })
}

/// The documents of a bitmap, each scoring 0: a `bool` of `filter` clauses without `must`.
pub(super) struct FilterDocs {
    docs: IntoIter,
    cost: u64,
    pos: Pos,
}

impl FilterDocs {
    pub(super) fn new(docs: RoaringBitmap) -> Self {
        Self {
            cost: docs.len(),
            docs: docs.into_iter(),
            pos: Pos::Unstarted,
        }
    }
}

impl Matcher for FilterDocs {
    fn cost(&self) -> u64 {
        self.cost
    }

    fn doc(&self) -> Option<u32> {
        self.pos.doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        if self.pos == Pos::Done {
            return Ok(None);
        }
        let doc = self.docs.next();
        self.pos = Pos::of(doc);
        Ok(doc)
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        match self.pos {
            Pos::At(d) if d >= target => return Ok(Some(d)),
            Pos::Done => return Ok(None),
            Pos::Unstarted | Pos::At(_) => {}
        }
        self.docs.advance_to(target);
        self.next()
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        Ok(0.0)
    }

    fn max_score(&self) -> f64 {
        0.0
    }
}

/// The documents of `inner` in a bitmap, scored by `inner`: a filter adds 0 to a score (SPEC §7.2), so the score, its
/// bounds and the threshold are `inner`'s.
pub(super) struct Filtered<M> {
    inner: M,
    docs: FilterDocs,
    pos: Pos,
}

impl<M: Matcher> Filtered<M> {
    pub(super) fn new(inner: M, docs: RoaringBitmap) -> Self {
        Self {
            inner,
            docs: FilterDocs::new(docs),
            pos: Pos::Unstarted,
        }
    }

    /// Leapfrogs `inner` and the bitmap from `target` to their first common document.
    fn align(&mut self, mut target: u32) -> Result<Option<u32>, ReadError> {
        let doc = loop {
            let Some(d) = self.inner.advance(target)? else {
                break None;
            };
            let Some(f) = self.docs.advance(d)? else {
                break None;
            };
            if f == d {
                break Some(d);
            }
            target = f;
        };
        self.pos = Pos::of(doc);
        Ok(doc)
    }
}

impl<M: Matcher> Matcher for Filtered<M> {
    fn cost(&self) -> u64 {
        self.inner.cost().min(self.docs.cost())
    }

    fn doc(&self) -> Option<u32> {
        self.pos.doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        let Some(target) = self.pos.next_target() else {
            self.pos = Pos::Done;
            return Ok(None);
        };
        self.align(target)
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        match self.pos {
            Pos::At(d) if d >= target => Ok(Some(d)),
            Pos::Done => Ok(None),
            Pos::Unstarted | Pos::At(_) => self.align(target),
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

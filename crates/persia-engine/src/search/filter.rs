//! The `filter` clauses of a `bool` (SPEC §7.1.2, §7.3, ROADMAP 7.4.5): a view evaluates them to a bitmap of the live
//! documents they select before any document is scored. [`FilterDocs`] matches that bitmap, scoring 0; [`Filtered`]
//! matches a matcher's documents in it with the matcher's scores and bounds, so a threshold reaches what it wraps.
//!
//! A `term` or `terms` filter on a `keyword` field reads the field's postings; on another field, and a `range`, it
//! scans the column's order-preserving keys (SPEC §5.2.1), skipping the FOR blocks its bounds rule out and taking
//! whole those they rule in ([`SearchView::select_keys`], ROADMAP 7.4.13); `exists` reads which documents have a value.
//! `not` takes the complement among the view's documents (`0..doc_count`, one roaring run per 2^16 ids). A count
//! above [`crate::segment::MAX_DOCS`] is corruption returned before that range is built. Deleted documents are taken
//! out once, at the end. A `bool` with few candidates may check its filters per candidate instead ([`super::check`]).

use core::cell::RefCell;
use core::ops::Bound;

use persia_format::for_codec::{f64_to_key, i64_to_key};
use roaring::RoaringBitmap;
use roaring::bitmap::IntoIter;

use super::{Matcher, Pos, ScoreBound};
use crate::bitmap::Ascending;
use crate::query::{Filter, FilterValue};
use crate::schema::FieldId;
use crate::segment::{Overlap, ReadError};
use crate::view::{PostingsIterator, SearchView};

/// The documents each `bool` of a plan selected with its filters in one view, by the `bool`'s slot in the plan:
/// what a search's matcher selected, handed to the explanation of the view's hits, which then does not select them
/// again (SPEC §7.7, ROADMAP 7.7.3). A `bool` whose filters a matcher did not read (its `must` clauses match
/// nothing in the view) has none; the explanation never reaches its filters either.
#[derive(Debug, Default)]
pub(crate) struct Selections(RefCell<Vec<Option<(RoaringBitmap, bool)>>>);

impl Selections {
    /// Keeps `docs`, the selection of the `bool` at `slot`: of every filter, or of the filters that read postings
    /// only when `checked` (the others were checked per candidate, ROADMAP 7.4.16).
    pub(super) fn keep(&self, slot: usize, docs: &RoaringBitmap, checked: bool) {
        let mut kept = self.0.borrow_mut();
        if kept.len() <= slot {
            kept.resize(slot.saturating_add(1), None);
        }
        if let Some(s) = kept.get_mut(slot) {
            *s = Some((docs.clone(), checked));
        }
    }

    /// The selection kept for the `bool` at `slot`, taken out, and whether it is of the postings filters only.
    pub(super) fn take(&self, slot: usize) -> Option<(RoaringBitmap, bool)> {
        self.0.borrow_mut().get_mut(slot).and_then(Option::take)
    }
}

/// The live documents of `view` that every filter of `filters` selects (none: every live document).
///
/// # Errors
/// A filter on a field `view` does not have with that type (a plan of another schema); a corruption of the view.
pub(super) fn select<V: SearchView>(
    filters: &[Filter],
    view: &V,
) -> Result<RoaringBitmap, ReadError> {
    let mut docs = all(filters, view)?;
    docs -= view.deleted()?;
    Ok(docs)
}

/// `docs` (live documents of `view`) narrowed to those every filter of `filters` selects.
///
/// # Errors
/// As [`select`].
pub(super) fn narrow<V: SearchView>(
    docs: &mut RoaringBitmap,
    filters: &[Filter],
    view: &V,
) -> Result<(), ReadError> {
    for filter in filters {
        if docs.is_empty() {
            break;
        }
        *docs &= eval(filter, view)?;
    }
    Ok(())
}

/// The documents every filter selects, deleted ones included.
fn all<V: SearchView>(filters: &[Filter], view: &V) -> Result<RoaringBitmap, ReadError> {
    let mut docs = every_doc(view)?;
    narrow(&mut docs, filters, view)?;
    Ok(docs)
}

fn every_doc<V: SearchView>(view: &V) -> Result<RoaringBitmap, ReadError> {
    // `doc_count` above `MAX_DOCS` is refused before the range is built (SPEC §5.4).
    crate::bitmap::doc_range(view.doc_count())
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
        Filter::Not(filter) => Ok(every_doc(view)? - eval(filter, view)?),
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
    let (mut trues, mut falses) = (false, false);
    for value in values {
        match value {
            FilterValue::Keyword(term) => {
                if let Some(mut postings) = view.postings(field, term.as_bytes())? {
                    let mut list = Ascending::default();
                    while let Some(docid) = postings.next()? {
                        list.push(docid);
                    }
                    docs |= list.finish();
                }
            }
            // From the column's bitmaps, without a key per document.
            FilterValue::Bool(true) => trues = true,
            FilterValue::Bool(false) => falses = true,
            other => keys.extend(key(other)),
        }
    }
    match (trues, falses) {
        (true, true) => docs |= view.present(field)?,
        (true, false) => docs |= view.trues(field)?,
        (false, true) => docs |= view.present(field)? - view.trues(field)?,
        (false, false) => {}
    }
    if !keys.is_empty() {
        keys.sort_unstable();
        keys.dedup();
        docs |= view.select_keys(
            field,
            |min, max| among(&keys, min, max),
            |k| keys.binary_search(&k).is_ok(),
        )?;
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
    match key_range(lower, upper) {
        Some((low, high)) => view.select_keys(
            field,
            |min, max| between(low, high, min, max),
            |k| (low..=high).contains(&k),
        ),
        None => Ok(RoaringBitmap::new()),
    }
}

/// The keys within the bounds, `low..=high`; `None` when no key is.
pub(super) fn key_range(
    lower: &Bound<FilterValue>,
    upper: &Bound<FilterValue>,
) -> Option<(u64, u64)> {
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
        (Some(low), Some(high)) if low <= high => Some((low, high)),
        _ => None,
    }
}

/// How keys in `min..=max` meet `low..=high`.
pub(super) fn between(low: u64, high: u64, min: u64, max: u64) -> Overlap {
    if max < low || min > high {
        Overlap::Disjoint
    } else if low <= min && max <= high {
        Overlap::Within
    } else {
        Overlap::Partial
    }
}

/// How keys in `min..=max` meet `keys` (sorted, distinct).
pub(super) fn among(keys: &[u64], min: u64, max: u64) -> Overlap {
    let from = keys.partition_point(|&k| k < min);
    match keys.get(from) {
        Some(&k) if k <= max => {
            if min == max {
                Overlap::Within
            } else {
                Overlap::Partial
            }
        }
        _ => Overlap::Disjoint,
    }
}

/// The column key of `value` (SPEC §5.2.1), as the builder stores it: `-0.0` is `0.0` (SPEC §6.4.1); `None` for a
/// `keyword`, which has postings instead.
pub(super) fn key(value: &FilterValue) -> Option<u64> {
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

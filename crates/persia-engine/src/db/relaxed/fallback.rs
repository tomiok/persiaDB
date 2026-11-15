//! L5 `fallback`'s own ranking (SPEC §7.6, ROADMAP 8.2.5, ADR-0044): the matches of its step (the documents the
//! filters keep, the excluded clauses do not match and no earlier step matched), best first by static rank (SPEC
//! §6.2.3, the hit's score), then recency, whatever the request's sort.
//!
//! Recency is where the index holds a document: the memtable first, then the segments from the last the manifest
//! lists to the first, each by docid descending. So a candidate ranks by `(static rank, view, docid)` descending,
//! with the memtable the greatest view: [`Key`]. Nothing is scored. With a static rank, each segment visits its FOR
//! blocks best first by the static rank of their bound and stops at the first that cannot beat the worst hit kept;
//! within a block, a key below the smallest that could is passed over without computing its static rank. The
//! memtable, which has no bounds, and a search counting facets walk every candidate in docid order.
//! By recency alone (every static rank 0), the newest views come first, each read from its last docid back, and the
//! walk stops once it has the hits it needs.

use core::cmp::Ordering;
use std::collections::BinaryHeap;

use roaring::RoaringBitmap;

use super::super::exec::Execution;
use super::super::search::{Collected, Cursor, Part, Ranked};
use super::super::snapshot::Located;
use super::super::views::CollectionViews;
use crate::bitmap::Ascending;
use crate::document::Value;
use crate::relax::FallbackOrder;
use crate::schema::{CollectionSchema, FieldId, FieldType, StaticRank};
use crate::score::{static_rank, static_rank_value};
use crate::search::{
    Explanation, FacetCounter, Interruptible, Matcher, RequestError, SearchPlan, Steps, sort_values,
};
use crate::segment::ReadError;
use crate::view::{KeyReader, SearchView};

/// The view rank of the memtable: after every segment, so the most recent.
const MEMTABLE: usize = usize::MAX;

/// The smallest window a recency walk reads back from a view's end.
const MIN_WINDOW: u32 = 64;

/// What L5 ranks by in a collection: its static rank's field and function, or recency alone.
#[derive(Debug, Clone, Copy)]
pub(super) enum Ranking<'s> {
    /// `rank` on `field`, a column of `kind` (`i64`, `f64` or `datetime`).
    StaticRank {
        rank: &'s StaticRank,
        field: FieldId,
        kind: FieldType,
    },
    /// Every static rank is 0: no static rank, or one of `weight` 0.
    Recency,
}

impl<'s> Ranking<'s> {
    /// The ranking of `schema`'s collection ([`FallbackOrder::of`]). A static rank whose field the schema lacks
    /// (validation forbids it) ranks by recency.
    pub(super) fn of(schema: &'s CollectionSchema) -> Self {
        let rank = schema.ranking().static_rank.as_ref();
        match rank.zip(rank.and_then(|r| schema.field(&r.field))) {
            Some((rank, f)) if FallbackOrder::of(schema.ranking()) == FallbackOrder::StaticRank => {
                Self::StaticRank {
                    rank,
                    field: f.id(),
                    kind: f.field_type(),
                }
            }
            _ => Self::Recency,
        }
    }

    /// The static rank of a document whose column key is `key` (`None`: no value), and the value it came from.
    fn score(self, key: Option<u64>) -> (f64, Option<f64>) {
        let Self::StaticRank { rank, kind, .. } = self else {
            return (0.0, None);
        };
        let value = key.and_then(|key| {
            let value = match kind {
                FieldType::F64 => Value::F64(persia_format::for_codec::key_to_f64(key)),
                FieldType::Datetime => Value::Datetime(persia_format::for_codec::key_to_i64(key)),
                _ => Value::I64(persia_format::for_codec::key_to_i64(key)),
            };
            static_rank_value(&value)
        });
        let value_ref = value.map(Value::F64);
        // `+ 0.0`: a `-0.0` (a weight of `-0.0`) ranks as `0.0`.
        (static_rank(rank, value_ref.as_ref()) + 0.0, value)
    }

    /// The largest key `block` may hold. Its bound is the minimum plus `2^width − 1` (SPEC §5.2.1), which for an `f64`
    /// column may pass the key of `f64::MAX` into those of `+∞` and NaN, whose static rank reads as 0; stored floats
    /// are finite (SPEC §6.2.2), so the bound stops at `f64::MAX`.
    fn largest(self, block: crate::segment::KeyBlock) -> u64 {
        match self {
            Self::StaticRank {
                kind: FieldType::F64,
                ..
            } => block
                .max
                .min(persia_format::for_codec::f64_to_key(f64::MAX))
                .max(block.min),
            _ => block.max,
        }
    }

    /// The smallest key of `block`'s range (`min..=max`) whose static rank is at least `score`, `None` when none is.
    /// The static rank never decreases with the key (key order is value order, SPEC §5.2.1, and `fn` and the weight are
    /// monotone, §6.2.3), so a binary search finds it, and every key below it has a lower static rank.
    fn threshold(self, block: crate::segment::KeyBlock, score: f64) -> Option<u64> {
        let reaches = |key: u64| self.score(Some(key)).0 >= score;
        let (mut lo, mut hi) = (block.min, self.largest(block));
        if lo > hi || !reaches(hi) {
            return None;
        }
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if reaches(mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Some(lo)
    }
}

/// A candidate's place in L5's order: greater ranks first. Static ranks are finite and never negative (SPEC §6.2.3).
#[derive(Debug, Clone, Copy)]
struct Key {
    score: f64,
    view: usize,
    docid: u32,
}

impl Key {
    fn location(self) -> Located {
        if self.view == MEMTABLE {
            Located::Memtable(self.docid)
        } else {
            Located::Segment {
                index: self.view,
                docid: self.docid,
            }
        }
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then(self.view.cmp(&other.view))
            .then(self.docid.cmp(&other.docid))
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Key {}

/// The best `k` keys offered, the worst on top. `Reverse` order: the heap's greatest is the worst key.
struct Best {
    k: usize,
    heap: BinaryHeap<core::cmp::Reverse<Key>>,
}

impl Best {
    fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k.min(1024)),
        }
    }

    fn full(&self) -> bool {
        self.heap.len() >= self.k
    }

    fn worst(&self) -> Option<Key> {
        self.heap.peek().map(|r| r.0)
    }

    /// Whether a key ranking at best `bound` could enter.
    fn admits(&self, bound: Key) -> bool {
        self.k > 0 && (!self.full() || self.worst().is_none_or(|w| bound > w))
    }

    fn offer(&mut self, key: Key) {
        if !self.admits(key) {
            return;
        }
        if self.full() {
            self.heap.pop();
        }
        self.heap.push(core::cmp::Reverse(key));
    }

    /// Best first.
    fn into_sorted(self) -> Vec<Key> {
        let mut keys: Vec<Key> = self.heap.into_iter().map(|r| r.0).collect();
        keys.sort_unstable_by(|a, b| b.cmp(a));
        keys
    }
}

/// What L5's search found: its hits, as any step's, and their explanations when the page is explained (in hit order).
pub(super) struct FallbackHits {
    pub(super) collected: Collected,
    pub(super) explanations: Vec<Option<Explanation>>,
}

impl CollectionViews<'_> {
    /// The best `part.window` matches of `plan` (L5's step with the earlier steps excluded) by L5's order,
    /// after `part.after` when given (a cursor ranks by its location's own key), with each view's matches counted for
    /// `part.facets`, under `exec` (its deadline, cancellation and, within the cascade, the budget).
    pub(super) fn collect_fallback(
        &self,
        plan: &SearchPlan,
        part: &Part<'_>,
        exec: &Execution,
    ) -> Result<FallbackHits, RequestError> {
        let ranking = Ranking::of(self.collection().schema());
        let segments: Vec<_> = self.segments().collect();
        let after = match part.after {
            Some(cursor) => Some(self.cursor_key(cursor, ranking, &segments)?),
            None => None,
        };
        let counting = !part.facets.is_empty();
        let mut best = Best::new(part.window);
        let mut facets: Vec<FacetCounter> =
            part.facets.iter().copied().map(FacetCounter::new).collect();
        let walk = Walk {
            plan,
            ranking,
            after,
            counting,
        };
        // Newest view first: by recency alone, once `best` is full no older view has a better key.
        let order = core::iter::once(MEMTABLE).chain((0..segments.len()).rev());
        for view in order {
            exec.interrupt()?;
            let recency_done = matches!(ranking, Ranking::Recency) && !counting && best.full();
            if recency_done {
                break;
            }
            let steps = Steps::new(exec);
            let docs = if view == MEMTABLE {
                walk.view(
                    &Interruptible::new(self.memtable(), &steps),
                    view,
                    &mut best,
                )?
            } else {
                let Some(segment) = segments.get(view) else {
                    continue;
                };
                walk.view(&Interruptible::new(segment, &steps), view, &mut best)?
            };
            if let Some(docs) = docs {
                for facet in &mut facets {
                    exec.interrupt()?;
                    if view == MEMTABLE {
                        facet.add(self.memtable(), &docs)?;
                    } else if let Some(segment) = segments.get(view) {
                        facet.add(segment, &docs)?;
                    }
                }
            }
        }
        // The hits' sort values (their cursors) and explanations, read in their views.
        let keys = best.into_sorted();
        let mut ranked = Vec::with_capacity(keys.len());
        let mut explanations = Vec::new();
        for key in keys {
            let (values, value) = if key.view == MEMTABLE {
                hit_values(self.memtable(), key, part, ranking)?
            } else {
                let segment = segments.get(key.view).ok_or(RequestError::HitSegment {
                    index: key.view,
                    segments: segments.len(),
                })?;
                hit_values(segment, key, part, ranking)?
            };
            if part.explain {
                explanations.push(Some(Explanation::StaticRank {
                    score: key.score,
                    value,
                }));
            }
            ranked.push(Ranked {
                location: key.location(),
                score: key.score,
                values,
            });
        }
        Ok(FallbackHits {
            collected: Collected {
                ranked,
                facets,
                kept: Vec::new(),
            },
            explanations,
        })
    }

    /// The key of a cursor's hit: its location, and the static rank read there (the same snapshot and memtable view
    /// as the cursor's, which [`Self::check_request`] checked).
    fn cursor_key(
        &self,
        cursor: &Cursor,
        ranking: Ranking<'_>,
        segments: &[crate::view::SegmentView<'_>],
    ) -> Result<Key, RequestError> {
        let (view, docid) = match cursor.location {
            Located::Memtable(docid) => (MEMTABLE, docid),
            Located::Segment { index, docid } => (index, docid),
        };
        // A location the views do not have is an error in either order, not a cursor that pages nothing.
        let segment = match view {
            MEMTABLE => None,
            index => Some(segments.get(index).ok_or(RequestError::HitSegment {
                index,
                segments: segments.len(),
            })?),
        };
        let key = match (ranking, segment) {
            (Ranking::Recency, _) => None,
            (Ranking::StaticRank { field, .. }, None) => self.memtable().keys(field)?.key(docid)?,
            (Ranking::StaticRank { field, .. }, Some(segment)) => {
                segment.keys(field)?.key(docid)?
            }
        };
        Ok(Key {
            score: ranking.score(key).0,
            view,
            docid,
        })
    }
}

/// A hit's sort values for the request's sort (its cursor), and the static rank field's value it scored with.
fn hit_values<V: SearchView>(
    view: &V,
    key: Key,
    part: &Part<'_>,
    ranking: Ranking<'_>,
) -> Result<(Vec<crate::search::SortValue>, Option<f64>), RequestError> {
    let values = sort_values(part.sort, view, key.docid, key.score)?;
    let value = match ranking {
        Ranking::StaticRank { field, .. } if part.explain => {
            ranking.score(view.keys(field)?.key(key.docid)?).1
        }
        _ => None,
    };
    Ok((values, value))
}

/// L5's walk of one view.
struct Walk<'a, 's> {
    plan: &'a SearchPlan,
    ranking: Ranking<'s>,
    /// Only keys below it rank after the cursor.
    after: Option<Key>,
    /// Facets count every candidate: no block or view is skipped.
    counting: bool,
}

impl Walk<'_, '_> {
    /// Offers `view`'s candidates (at `rank` in collection order) to `best`; with facets, returns them all.
    fn view<V: SearchView>(
        &self,
        view: &V,
        rank: usize,
        best: &mut Best,
    ) -> Result<Option<RoaringBitmap>, RequestError> {
        match self.ranking {
            _ if self.counting => self.forward(view, rank, best),
            Ranking::Recency => {
                self.newest(view, rank, best)?;
                Ok(None)
            }
            Ranking::StaticRank { field, .. } => {
                if !self.by_blocks(view, rank, field, best)? {
                    return self.forward(view, rank, best);
                }
                Ok(None)
            }
        }
    }

    /// With a static rank and no facet: `view`'s FOR blocks best first, each by the best key it can hold (the static
    /// rank of its largest value, at its last docid), until one cannot beat the worst kept key, which no later block
    /// can either. Each block visited is walked by a matcher advanced to its start. `false` when the view has no bounds
    /// (the memtable, or a column without values), which [`Self::forward`] walks instead.
    fn by_blocks<V: SearchView>(
        &self,
        view: &V,
        rank: usize,
        field: FieldId,
        best: &mut Best,
    ) -> Result<bool, RequestError> {
        let mut keys = view.keys(field)?;
        let mut blocks = Vec::new();
        let mut docid = 0;
        while docid < view.doc_count() {
            let Some(block) = keys.block(docid)? else {
                return Ok(false);
            };
            if block.end <= docid {
                // A block that does not hold the docid asked for: no bounds to trust.
                return Ok(false);
            }
            let top = Key {
                score: self.ranking.score(Some(self.ranking.largest(block))).0,
                view: rank,
                docid: block.end.saturating_sub(1),
            };
            blocks.push((top, block));
            docid = block.end;
        }
        blocks.sort_unstable_by_key(|&(top, _)| core::cmp::Reverse(top));
        for (top, block) in blocks {
            if !best.admits(top) {
                break;
            }
            // Blocks are visited by bound, later docids first among equal ones, never ahead of a matcher already
            // walked: each gets a matcher of its own (ROADMAP 8.2.9 counts that cost).
            let mut matcher = self.plan.matcher(view)?;
            let mut next = matcher.advance(block.start)?;
            // The block's smallest key whose static rank reaches the worst kept hit's, for that worst hit: a key below
            // it ranks lower, and is passed over without computing its static rank.
            let mut floor: Option<(Key, Option<u64>)> = None;
            while let Some(docid) = next.filter(|&d| d < block.end) {
                next = matcher.next()?;
                let raw = keys.key(docid)?;
                if best.full()
                    && let Some(worst) = best.worst()
                {
                    let threshold = match floor {
                        Some((w, t)) if w == worst => t,
                        _ => {
                            let t = self.ranking.threshold(block, worst.score);
                            floor = Some((worst, t));
                            t
                        }
                    };
                    let loses = match raw {
                        Some(k) => threshold.is_none_or(|t| k < t),
                        None => worst.score > 0.0,
                    };
                    if loses {
                        continue;
                    }
                }
                let key = Key {
                    score: self.ranking.score(raw).0,
                    view: rank,
                    docid,
                };
                if self.after.is_none_or(|after| key < after) {
                    best.offer(key);
                }
            }
        }
        Ok(true)
    }

    /// Every candidate in docid order, its static rank read from the column: a view without bounds (the memtable, a
    /// column no document has a value in), and any view when facets count every candidate.
    fn forward<V: SearchView>(
        &self,
        view: &V,
        rank: usize,
        best: &mut Best,
    ) -> Result<Option<RoaringBitmap>, RequestError> {
        let mut matcher = self.plan.matcher(view)?;
        let mut keys = match self.ranking {
            Ranking::StaticRank { field, .. } => Some(view.keys(field)?),
            Ranking::Recency => None,
        };
        let mut docs = Ascending::default();
        let mut next = matcher.next()?;
        while let Some(docid) = next {
            if self.counting {
                docs.push(docid);
            }
            let key = Key {
                score: match keys.as_mut() {
                    Some(keys) => self.ranking.score(keys.key(docid)?).0,
                    None => 0.0,
                },
                view: rank,
                docid,
            };
            if self.after.is_none_or(|after| key < after) {
                best.offer(key);
            }
            next = matcher.next()?;
        }
        Ok(self.counting.then(|| docs.finish()))
    }

    /// By recency alone: the newest candidates of `view` (below the cursor's docid when it is the cursor's view; none
    /// when it is newer than the cursor's), until `best` is full, reading back from the view's end in windows that
    /// double. Every key here is better than any of an older view, which is walked only if `best` still has room.
    fn newest<V: SearchView>(
        &self,
        view: &V,
        rank: usize,
        best: &mut Best,
    ) -> Result<(), ReadError> {
        let mut end = view.doc_count();
        if let Some(after) = self.after {
            match rank.cmp(&after.view) {
                Ordering::Greater => return Ok(()),
                Ordering::Equal => end = end.min(after.docid),
                Ordering::Less => {}
            }
        }
        let mut window = MIN_WINDOW;
        while end > 0 && !best.full() {
            let start = end.saturating_sub(window);
            let mut matcher = self.plan.matcher(view)?;
            let mut found = Vec::new();
            let mut next = matcher.advance(start)?;
            while let Some(docid) = next.filter(|&d| d < end) {
                found.push(docid);
                next = matcher.next()?;
            }
            for &docid in found.iter().rev() {
                if best.full() {
                    break;
                }
                best.offer(Key {
                    score: 0.0,
                    view: rank,
                    docid,
                });
            }
            end = start;
            window = window.saturating_mul(2);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

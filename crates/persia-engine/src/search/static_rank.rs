//! The collection's static rank in every query hit's score (SPEC §6.2.3, §7.2, ROADMAP 10.1.45): a matcher around a
//! plan's root that adds `weight · fn(v)` to each document's score, the query's score first, then the contribution.
//!
//! Pruning stays exact (SPEC §7.3). A view's contributions are bounded by `m`, the contribution of the largest key
//! each of its column's FOR blocks may hold (the contribution never decreases with the key). A document's score is
//! `sat(q ⊕ s)` with `q` its query score and `s ≤ m` its contribution, so `sat(Q ⊕ m)` bounds it whenever `Q`
//! bounds `q` (floating-point addition and saturation are monotonic). A threshold `t` on the whole score becomes the
//! largest `t'` found with `sat(t' ⊕ m) ≤ t` for the query's matchers: a document they skip has `q ≤ t'`, so its whole
//! score is at most `t`, which the threshold allowed skipping. A view whose column has no block bounds (the memtable,
//! a column without values) has no `m`: it is searched without a threshold.

use crate::schema::{CollectionSchema, FieldId, FieldType, StaticRank};
use crate::score::{key_contribution, largest_key, saturate};
use crate::segment::ReadError;
use crate::view::{KeyReader, SearchView};

use super::{Matcher, ScoreBound};

/// What a plan adds to each hit: the collection's static rank on its field's column.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RankPlan {
    rank: StaticRank,
    field: FieldId,
    kind: FieldType,
}

impl RankPlan {
    /// The static rank of `schema`'s collection; `None` without one, or with one of `weight` 0 (or not above 0),
    /// which adds 0 to every score, or on a field the schema lacks (validation forbids it).
    pub(super) fn of(schema: &CollectionSchema) -> Option<Self> {
        let rank = schema.ranking().static_rank.as_ref()?;
        let field = schema.field(&rank.field)?;
        // NaN too, which validation forbids.
        if rank.weight.is_nan() || rank.weight <= 0.0 {
            return None;
        }
        Some(Self {
            rank: rank.clone(),
            field: field.id(),
            kind: field.field_type(),
        })
    }

    /// The contribution of `docid` of a view whose static rank column `keys` reads, and the value it came from.
    pub(super) fn contribution(
        &self,
        keys: &mut impl KeyReader,
        docid: u32,
    ) -> Result<(f64, Option<f64>), ReadError> {
        Ok(key_contribution(&self.rank, self.kind, keys.key(docid)?))
    }

    /// The column of `view` the contributions are read from.
    pub(super) fn keys<'v, V: SearchView>(&self, view: &'v V) -> Result<V::Keys<'v>, ReadError> {
        view.keys(self.field)
    }

    /// The largest contribution of a document of a view of `doc_count` documents whose column `keys` reads, from its
    /// FOR blocks' bounds; `None` when a block has none (the memtable, a column without values).
    fn bound(&self, keys: &mut impl KeyReader, doc_count: u32) -> Result<Option<f64>, ReadError> {
        let mut max = 0.0_f64;
        let mut docid = 0;
        while docid < doc_count {
            let Some(block) = keys.block(docid)? else {
                return Ok(None);
            };
            if block.end <= docid || block.start > docid {
                // A block that does not hold the docid asked for: no bounds to trust.
                return Ok(None);
            }
            let (top, _) =
                key_contribution(&self.rank, self.kind, Some(largest_key(self.kind, block)));
            max = max.max(top);
            docid = block.end;
        }
        Ok(Some(max))
    }
}

/// `query ⊕ contribution`, saturated at `f64::MAX` (SPEC §7.2): the whole score of a hit.
pub(super) fn total(query: f64, contribution: f64) -> f64 {
    saturate(query + contribution)
}

/// The largest threshold `t'` found with `total(t', bound) ≤ t`, for the query's matchers under a threshold `t` on the
/// whole score; `None` when there is none to pass (NaN, or a sum that cannot get back under `t`).
fn query_threshold(t: f64, bound: f64) -> Option<f64> {
    if t.is_nan() {
        return None;
    }
    if bound == 0.0 {
        return Some(t);
    }
    let mut x = t - bound;
    // `t − bound` is correctly rounded, so the sum overshoots `t` by at most an ulp or two.
    for _ in 0..4 {
        if !x.is_finite() {
            return None;
        }
        if total(x, bound) <= t {
            return Some(x);
        }
        x = x.next_down();
    }
    None
}

/// A plan's root matcher with the static rank added to its scores (the module's doc).
pub(super) struct StaticRanked<'a, V: SearchView> {
    inner: Box<dyn Matcher + 'a>,
    plan: &'a RankPlan,
    view: &'a V,
    /// Opened at the first score or bound: a walk that never scores (L5's, a facet count's) reads no key.
    keys: Option<V::Keys<'a>>,
    /// Whether [`Self::bound`] holds the view's bound yet.
    bound_read: bool,
    /// The view's largest contribution, once read; `None` when the view has no bounds.
    bound: Option<f64>,
    /// A read of the bounds that failed where no error could be returned ([`Matcher::set_min_competitive`]): the next
    /// move returns it, so the search fails as on any corrupt read.
    failed: Option<ReadError>,
}

impl<'a, V: SearchView + 'a> StaticRanked<'a, V> {
    pub(super) fn new(inner: Box<dyn Matcher + 'a>, plan: &'a RankPlan, view: &'a V) -> Self {
        Self {
            inner,
            plan,
            view,
            keys: None,
            bound_read: false,
            bound: None,
            failed: None,
        }
    }

    fn keys(&mut self) -> Result<&mut V::Keys<'a>, ReadError> {
        match &mut self.keys {
            Some(keys) => Ok(keys),
            slot => Ok(slot.insert(self.plan.keys(self.view)?)),
        }
    }

    /// The view's largest contribution, read once; `None` when it has no bounds.
    ///
    /// # Errors
    /// A corruption of the column.
    fn bound(&mut self) -> Result<Option<f64>, ReadError> {
        if self.bound_read {
            return Ok(self.bound);
        }
        let doc_count = self.view.doc_count();
        let plan = self.plan;
        self.bound = plan.bound(self.keys()?, doc_count)?;
        self.bound_read = true;
        Ok(self.bound)
    }

    /// The failure of a read [`Matcher::set_min_competitive`] could not return, if any.
    fn failed(&mut self) -> Result<(), ReadError> {
        self.failed.take().map_or(Ok(()), Err)
    }
}

impl<'a, V: SearchView + 'a> Matcher for StaticRanked<'a, V> {
    fn cost(&self) -> u64 {
        self.inner.cost()
    }

    fn doc(&self) -> Option<u32> {
        self.inner.doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        self.failed()?;
        self.inner.next()
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        self.failed()?;
        self.inner.advance(target)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        let Some(docid) = self.inner.doc() else {
            return Ok(0.0);
        };
        let query = self.inner.score()?;
        let plan = self.plan;
        let (contribution, _) = plan.contribution(self.keys()?, docid)?;
        Ok(total(query, contribution))
    }

    fn max_score(&self) -> f64 {
        match self.bound {
            Some(bound) => total(self.inner.max_score(), bound),
            // Not read yet (a `&self` cannot read the column), or no bounds: nothing is known below `f64::MAX`.
            _ => f64::MAX,
        }
    }

    fn block_bound(&mut self, target: u32) -> Result<Option<ScoreBound>, ReadError> {
        let bound = self.bound()?.unwrap_or(f64::MAX);
        Ok(self.inner.block_bound(target)?.map(|b| ScoreBound {
            up_to: b.up_to,
            score: total(b.score, bound),
        }))
    }

    fn set_min_competitive(&mut self, score: f64) {
        match self.bound() {
            Ok(Some(bound)) => {
                if let Some(query) = query_threshold(score, bound) {
                    self.inner.set_min_competitive(query);
                }
            }
            // No bounds: the view is searched without a threshold.
            Ok(None) => {}
            Err(e) => self.failed = Some(e),
        }
    }

    fn field_positions(&mut self, field: FieldId) -> Result<&[u32], ReadError> {
        self.inner.field_positions(field)
    }
}

#[cfg(test)]
mod tests {
    use super::{query_threshold, total};

    /// The threshold the query's matchers get never lets through a query score whose whole score beats the outer
    /// threshold, at any bound, also near `f64::MAX`.
    #[test]
    #[allow(clippy::cast_precision_loss)] // 53-bit values, exact
    fn query_thresholds_keep_the_whole_score_at_most_the_threshold() {
        let mut x = 0x9E37_79B9_7F4A_7C15_u64;
        let mut unit = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1_u64 << 53) as f64
        };
        let mut found = 0;
        for _ in 0..100_000 {
            let t = unit() * 50.0;
            let bound = unit() * 20.0;
            if let Some(q) = query_threshold(t, bound) {
                found += 1;
                assert!(total(q, bound) <= t, "{q} + {bound} > {t}");
            }
        }
        assert!(found > 99_000);
        assert_eq!(query_threshold(3.0, 0.0), Some(3.0));
        assert_eq!(query_threshold(f64::NAN, 1.0), None);
        // A threshold below the bound: the query score would have to be negative, which none is.
        let q = query_threshold(1.0, 2.0).unwrap();
        assert!(q < 0.0 && total(q, 2.0) <= 1.0);
        // Saturation: every sum is at most `f64::MAX`.
        let q = query_threshold(f64::MAX, f64::MAX).unwrap();
        assert!(total(q, f64::MAX) <= f64::MAX);
    }
}

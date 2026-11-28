//! The collection's static rank in every query hit's score (SPEC §6.2.3, §7.2, ROADMAP 10.1.45): a matcher around a
//! plan's root that adds `weight · fn(v)` to each document's score, the query's score first, then the contribution.
//!
//! Pruning stays exact (SPEC §7.3). A segment's contributions are bounded by `m`, the contribution of the largest key
//! its column's FOR blocks may hold (the contribution never decreases with the key), read from the blocks' bounds once
//! per segment, which never changes, and kept on it (`Segment::largest_key`); a column without values bounds them
//! by 0. A document's score is `sat(q ⊕ s)` with `q` its query score and `s ≤ m` its contribution, so `sat(Q ⊕ m)`
//! bounds it whenever `Q` bounds `q` (floating-point addition and saturation are monotonic). A threshold `t` on the
//! whole score becomes the largest `t'` found with `sat(t' ⊕ m) ≤ t` for the query's matchers: a document they skip
//! has `q ≤ t'`, so its whole score is at most `t`, which the threshold allowed skipping. A view without block bounds
//! (the memtable) has no `m`: it is searched without a threshold. Nor is an exhaustive search, whose matchers ignore
//! one, so it reads no `m`.

use crate::schema::{CollectionSchema, FieldId, FieldType, StaticRank};
use crate::score::{key_contribution, largest_contribution, saturate};
use crate::segment::{ReadError, SectionKind, SegmentCorruption};
use crate::view::{KeyReader, SearchView};

use super::{Matcher, Pruning, ScoreBound};

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

    /// The largest contribution of a document of a column no key of which is above `largest` (SPEC §7.3); `None`
    /// for a field whose keys are not ordered as the contribution (validation allows only `i64`, `f64` and
    /// `datetime`).
    fn bound(&self, largest: u64) -> Option<f64> {
        matches!(
            self.kind,
            FieldType::I64 | FieldType::F64 | FieldType::Datetime
        )
        .then(|| largest_contribution(&self.rank, self.kind, largest))
    }
}

/// A key no key of a segment column of `doc_count` documents that `keys` reads is above: the largest its FOR blocks'
/// bounds allow (SPEC §5.2.1), 0 when no document has a value (the column has no keys). The contribution never
/// decreases with the key, so this key's contribution bounds every document's. Uncapped, unlike
/// [`crate::score::largest_key`]'s per block: [`RankPlan::bound`] caps it.
///
/// # Errors
/// A corruption of a block's header or position: a block that does not hold the docid asked for, or none after one
/// that did (a segment column has blocks for all its documents or for none), is corruption too, not a bound.
fn column_largest_key(
    keys: &mut impl KeyReader,
    field: FieldId,
    doc_count: u32,
) -> Result<u64, ReadError> {
    let mut largest = 0;
    let mut docid = 0;
    while docid < doc_count {
        let Some(block) = keys.block(docid)? else {
            if docid == 0 {
                // No blocks at all: no document has a value.
                return Ok(0);
            }
            return Err(column_corruption(
                field,
                docid,
                "a gap in a column's FOR blocks",
            ));
        };
        if block.end <= docid || block.start > docid {
            return Err(column_corruption(
                field,
                docid,
                "a FOR block that does not hold its docid",
            ));
        }
        largest = largest.max(block.max);
        docid = block.end;
    }
    Ok(largest)
}

/// The corruption `what` of `field`'s column, at `docid`.
fn column_corruption(field: FieldId, docid: u32, what: &'static str) -> ReadError {
    ReadError::Section {
        kind: SectionKind::Columns,
        field: field.get(),
        position: u64::from(docid),
        reason: SegmentCorruption::Invalid { what },
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
    /// How the query's matchers prune: under [`Pruning::Exhaustive`] each of them ignores a threshold, so none is
    /// passed on and no bound is read for one.
    pruning: Pruning,
    /// A read of the bounds that failed where no error could be returned ([`Matcher::set_min_competitive`]): the next
    /// move returns it, so the search fails as on any corrupt read.
    failed: Option<ReadError>,
}

impl<'a, V: SearchView + 'a> StaticRanked<'a, V> {
    pub(super) fn new(
        inner: Box<dyn Matcher + 'a>,
        plan: &'a RankPlan,
        view: &'a V,
        pruning: Pruning,
    ) -> Self {
        Self {
            inner,
            plan,
            view,
            keys: None,
            bound_read: false,
            bound: None,
            pruning,
            failed: None,
        }
    }

    fn keys(&mut self) -> Result<&mut V::Keys<'a>, ReadError> {
        match &mut self.keys {
            Some(keys) => Ok(keys),
            slot => Ok(slot.insert(self.plan.keys(self.view)?)),
        }
    }

    /// The view's largest contribution, read once per matcher from the segment, which reads its column's block
    /// bounds once ([`column_largest_key`]); `None` for a view without block bounds (the memtable).
    ///
    /// # Errors
    /// A corruption of the column, every time it is asked until a read succeeds.
    fn bound(&mut self) -> Result<Option<f64>, ReadError> {
        if self.bound_read {
            return Ok(self.bound);
        }
        let view = self.view;
        let plan = self.plan;
        let Some(segment) = view.as_segment() else {
            self.bound_read = true;
            return Ok(None);
        };
        let doc_count = view.doc_count();
        let largest = segment.largest_key(plan.field, || {
            column_largest_key(self.keys()?, plan.field, doc_count)
        })?;
        self.bound = plan.bound(largest);
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
        self.failed()?;
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
        // Every matcher built exhaustive ignores a threshold (SPEC §7.3): reading the bound would only cost, and fail
        // on a corrupt column a search that never prunes by it.
        if self.pruning == Pruning::Exhaustive {
            return;
        }
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
#[allow(clippy::cast_precision_loss)] // values below 2^53, exact
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use persia_analysis::AnalyzerRegistry;
    use persia_format::ByteSource;

    use super::{RankPlan, StaticRanked, column_largest_key, query_threshold, total};
    use crate::document::{Document, Value};
    use crate::schema::FieldId;
    use crate::schema::{
        CollectionSchema, FieldKind, RankFn, RankingConfig, SchemaBuilder, StaticRank, TextOptions,
    };
    use crate::search::{Matcher, Pruning, ScoreBound};
    use crate::segment::{
        KeyBlock, ReadError, SectionKind, Segment, SegmentBuilder, SegmentCorruption, SegmentMeta,
        SegmentWriter,
    };
    use crate::view::KeyReader;
    use crate::view::SegmentView;

    /// The query's matcher: documents `docs`, each scoring 1, bounded by 2 (1.5 per block of 5 docids), recording
    /// the last threshold it is given.
    struct Query {
        docs: Vec<u32>,
        at: Option<usize>,
        threshold: Rc<Cell<Option<f64>>>,
    }

    impl Matcher for Query {
        fn cost(&self) -> u64 {
            u64::try_from(self.docs.len()).unwrap_or(u64::MAX)
        }

        fn doc(&self) -> Option<u32> {
            self.at.and_then(|i| self.docs.get(i).copied())
        }

        fn next(&mut self) -> Result<Option<u32>, ReadError> {
            self.at = Some(self.at.map_or(0, |i| i + 1));
            Ok(self.doc())
        }

        fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
            while self.doc().is_none_or(|d| d < target) && self.next()?.is_some() {}
            Ok(self.doc())
        }

        fn score(&mut self) -> Result<f64, ReadError> {
            Ok(1.0)
        }

        fn max_score(&self) -> f64 {
            2.0
        }

        fn block_bound(&mut self, target: u32) -> Result<Option<ScoreBound>, ReadError> {
            Ok(Some(ScoreBound {
                up_to: target.saturating_add(5),
                score: 1.5,
            }))
        }

        fn set_min_competitive(&mut self, score: f64) {
            self.threshold.set(Some(score));
        }
    }

    fn ranked_schema() -> CollectionSchema {
        SchemaBuilder::new("c")
            .field("body", FieldKind::Text(TextOptions::default()))
            .field("sales", FieldKind::I64 { stored: false })
            .ranking(
                RankingConfig::default().static_rank(Some(
                    StaticRank::new("sales")
                        .function(RankFn::Linear)
                        .weight(0.5),
                )),
            )
            .build()
            .unwrap()
    }

    /// A segment of 2 000 documents (two FOR blocks), document `i` selling `sales(i)`.
    fn segment(schema: &CollectionSchema, sales: impl Fn(i64) -> Option<i64>) -> Segment {
        let body = schema.field("body").unwrap().id();
        let field = schema.field("sales").unwrap().id();
        let mut b = SegmentBuilder::new(schema, &AnalyzerRegistry::builtin()).unwrap();
        for i in 0..2_000 {
            let mut doc = Document::new(format!("d{i}")).with(body, Value::String("red".into()));
            if let Some(s) = sales(i) {
                doc = doc.with(field, Value::I64(s));
            }
            b.add(&doc).unwrap();
        }
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
        Segment::open(ByteSource::new(bytes.into()), schema).unwrap()
    }

    fn ranked<'a>(
        plan: &'a RankPlan,
        view: &'a SegmentView<'a>,
    ) -> (StaticRanked<'a, SegmentView<'a>>, Rc<Cell<Option<f64>>>) {
        let threshold = Rc::new(Cell::new(None));
        let query = Query {
            docs: vec![3, 1_500],
            at: None,
            threshold: Rc::clone(&threshold),
        };
        (
            StaticRanked::new(Box::new(query), plan, view, Pruning::MaxScore),
            threshold,
        )
    }

    /// The bounds add the largest contribution the column's FOR blocks allow, read once and kept on the segment; a
    /// threshold reaches the query less it, so it prunes. `max_score` and `block_bound` have no caller in a search
    /// (the wrapper is the root), so this is their test.
    #[test]
    fn bounds_add_the_columns_largest_contribution_kept_on_the_segment() {
        let schema = ranked_schema();
        let plan = RankPlan::of(&schema).unwrap();
        let field = schema.field("sales").unwrap().id();
        let segment = segment(&schema, |i| (i % 7 != 0).then_some(i));
        // What the blocks' headers allow: at least the largest value, 1 999.
        let mut keys = segment.numeric_column(field).unwrap().into_keys();
        let largest = [0, 1_024]
            .into_iter()
            .map(|d| keys.block(d).unwrap().unwrap().max)
            .max()
            .unwrap();
        let m = 0.5 * persia_format::for_codec::key_to_i64(largest) as f64;
        assert!(m >= 0.5 * 1_999.0, "{m}");
        let view = SegmentView::new(&segment, None);
        let (mut matcher, threshold) = ranked(&plan, &view);
        // Nothing read yet: no bound below `f64::MAX`.
        assert_eq!(matcher.max_score(), f64::MAX);
        assert_eq!(
            matcher.block_bound(7).unwrap(),
            Some(ScoreBound {
                up_to: 12,
                score: total(1.5, m),
            })
        );
        assert_eq!(matcher.max_score(), total(2.0, m));
        // The segment keeps the key: asking again reads nothing (this read would fail).
        assert_eq!(
            segment
                .largest_key(field, || Err(ReadError::ForeignTermInfo))
                .unwrap(),
            largest
        );
        matcher.set_min_competitive(m + 1.25);
        let t = threshold.get().unwrap();
        assert!(total(t, m) <= m + 1.25 && t > 1.0, "{t}");
        // A document scores the query's score plus its own contribution.
        assert_eq!(matcher.next().unwrap(), Some(3));
        assert_eq!(matcher.score().unwrap(), total(1.0, 1.5));
        // Another matcher on the segment takes the kept bound.
        let (mut again, _) = ranked(&plan, &view);
        assert_eq!(again.block_bound(0).unwrap().unwrap().score, total(1.5, m));
    }

    /// A failed read of the bound is not kept: the segment reads again the next time it is asked.
    #[test]
    fn a_failed_read_of_the_largest_key_is_not_kept() {
        let schema = ranked_schema();
        let field = schema.field("sales").unwrap().id();
        let segment = segment(&schema, Some);
        for _ in 0..2 {
            assert!(matches!(
                segment.largest_key(field, || Err(ReadError::ForeignTermInfo)),
                Err(ReadError::ForeignTermInfo)
            ));
        }
        assert_eq!(segment.largest_key(field, || Ok(7)).unwrap(), 7);
        assert_eq!(segment.largest_key(field, || Ok(8)).unwrap(), 7);
    }

    /// A column without values bounds the contribution by 0: the threshold reaches the query as it is, so a segment
    /// indexed before the field was ranked by still prunes.
    #[test]
    fn a_column_without_values_bounds_the_contribution_by_zero() {
        let schema = ranked_schema();
        let plan = RankPlan::of(&schema).unwrap();
        let segment = segment(&schema, |_| None);
        let view = SegmentView::new(&segment, None);
        let (mut matcher, threshold) = ranked(&plan, &view);
        matcher.set_min_competitive(1.25);
        assert_eq!(threshold.get(), Some(1.25));
        assert_eq!(matcher.max_score(), 2.0);
        assert_eq!(matcher.next().unwrap(), Some(3));
        assert_eq!(matcher.score().unwrap(), 1.0);
    }

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

    /// Blocks a test lays out: the one starting at the docid asked for, else the first starting after it (a block that
    /// does not hold that docid); `None` past the last, or for every docid in `missing`.
    struct Blocks {
        blocks: Vec<KeyBlock>,
        missing: Vec<u32>,
    }

    impl KeyReader for Blocks {
        fn key(&mut self, _docid: u32) -> Result<Option<u64>, ReadError> {
            Ok(None)
        }

        fn block(&mut self, docid: u32) -> Result<Option<KeyBlock>, ReadError> {
            if self.missing.contains(&docid) {
                return Ok(None);
            }
            let mut blocks = self.blocks.iter().copied();
            Ok(blocks
                .clone()
                .find(|b| b.start == docid)
                .or_else(|| blocks.find(|b| b.start > docid)))
        }
    }

    fn key_block(start: u32, end: u32, max: u64) -> KeyBlock {
        KeyBlock {
            start,
            end,
            min: 0,
            max,
        }
    }

    /// The walk takes the largest bound of blocks covering the column; a column without blocks is 0; a gap after a
    /// block, or a block that does not hold the docid asked for, is corruption, not a bound that would be kept.
    #[test]
    fn the_column_walk_takes_the_largest_bound_and_refuses_a_broken_layout() {
        let field = FieldId::new(3);
        let mut whole = Blocks {
            blocks: vec![key_block(0, 4, 9), key_block(4, 6, 20), key_block(6, 7, 5)],
            missing: vec![],
        };
        assert_eq!(column_largest_key(&mut whole, field, 7).unwrap(), 20);
        let mut none = Blocks {
            blocks: vec![],
            missing: vec![],
        };
        assert_eq!(column_largest_key(&mut none, field, 7).unwrap(), 0);
        let mut gap = Blocks {
            blocks: vec![key_block(0, 4, 9), key_block(4, 6, 20)],
            missing: vec![4],
        };
        let mut misplaced = Blocks {
            blocks: vec![key_block(0, 4, 9), key_block(4, 4, 20)],
            missing: vec![],
        };
        // The block given for docid 4 starts at 5.
        let mut late = Blocks {
            blocks: vec![key_block(0, 4, 9), key_block(5, 7, 20)],
            missing: vec![],
        };
        for (keys, at, what) in [
            (&mut gap, 4, "a gap in a column's FOR blocks"),
            (
                &mut misplaced,
                4,
                "a FOR block that does not hold its docid",
            ),
            (&mut late, 4, "a FOR block that does not hold its docid"),
        ] {
            let err = column_largest_key(keys, field, 7).unwrap_err();
            assert!(
                matches!(
                    &err,
                    ReadError::Section {
                        kind: SectionKind::Columns,
                        field: 3,
                        position,
                        reason: SegmentCorruption::Invalid { what: w },
                    } if *position == at && *w == what
                ),
                "{err:?}"
            );
            assert!(err.is_corruption());
        }
    }
}

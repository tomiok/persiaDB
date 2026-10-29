//! Collection statistics: the `N`, `df` and `avglen` BM25F scores with (SPEC §7.2, ROADMAP 7.2).
//!
//! They are collection-level: summed over every segment of a snapshot, deleted documents included until a merge
//! drops them (SPEC §5.5), plus the memtable's live documents. A [`StatisticsProvider`] gives them to the scorer
//! ([`crate::score`]); [`SegmentStatistics`] is the part of a snapshot's segments, computed once per snapshot
//! ([`crate::db::CollectionSnapshot::statistics`]), and [`SearchStatistics`] adds a memtable view to it. A provider
//! that sums them across shards (`global_stats`, SPEC §11.5, ROADMAP M13) implements the same trait.
//!
//! ```
//! use std::sync::Arc;
//! use persia_analysis::AnalyzerRegistry;
//! use persia_engine::document::{Document, Value};
//! use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
//! use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
//! use persia_engine::stats::{SegmentStatistics, StatisticsProvider};
//! use persia_format::ByteSource;
//!
//! let schema = SchemaBuilder::new("c").field("body", FieldKind::Text(TextOptions::default())).build()?;
//! let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
//! let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
//! b.add(&Document::new("a").with(body, Value::String("red shoes".into())))?;
//! b.add(&Document::new("b").with(body, Value::String("red red hat on sale".into())))?;
//! let meta = SegmentMeta { segment_id: 1, created_at: 0, merge_generation: 0 };
//! let segment = Segment::open(ByteSource::new(SegmentWriter::new(&b, meta).finish()?.into()), &schema)?;
//!
//! let stats = SegmentStatistics::new(&[Arc::new(segment)]);
//! assert_eq!(stats.doc_count(), 2);
//! assert_eq!(stats.field(body).avglen(), 3.5);
//! assert_eq!((stats.df(body, b"red")?, stats.df(body, b"blue")?), (2, 0));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, PoisonError, RwLock};

use crate::memtable::MemtableView;
use crate::schema::FieldId;
use crate::segment::{FieldStats, ReadError, Segment};
use crate::view::{PostingsIterator, SearchView};

/// Bytes of terms the `df` cache of a [`SegmentStatistics`] holds at most; past it, `df` is summed on each call.
const DF_CACHE_BYTES: usize = 4 << 20;

/// The statistics of one `text` or `keyword` field over a collection (SPEC §5.1, §7.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldTotals {
    /// Documents with at least one token (`text`) or value (`keyword`) in the field.
    pub docs_with_field: u64,
    /// Sum of their field lengths (SPEC §5.3.4).
    pub sum_field_len: u64,
}

impl FieldTotals {
    /// `avglen_f` of SPEC §7.2: the mean length of the field over the documents that have it; 1 when none has it
    /// (no document then holds one of its terms, so it only shapes score bounds).
    #[allow(clippy::cast_precision_loss)] // counts far below 2^52; a rounded mean is still a mean
    pub fn avglen(&self) -> f64 {
        let avg = self.sum_field_len as f64 / self.docs_with_field as f64;
        if avg.is_finite() && avg > 0.0 {
            avg
        } else {
            1.0
        }
    }

    fn add(self, stats: FieldStats) -> Self {
        Self {
            docs_with_field: self
                .docs_with_field
                .saturating_add(u64::from(stats.docs_with_field)),
            sum_field_len: self.sum_field_len.saturating_add(stats.sum_field_len),
        }
    }
}

/// The collection statistics BM25F scores with (SPEC §7.2): local to a snapshot, or global across shards (M13).
pub trait StatisticsProvider {
    /// `N`: the collection's documents, deleted ones included until a merge drops them (SPEC §7.2).
    fn doc_count(&self) -> u64;

    /// The totals of a `text` or `keyword` field; zero for another field or one no document has.
    fn field(&self, field: FieldId) -> FieldTotals;

    /// `df`: the documents holding `term` in `field`, counted as [`Self::doc_count`] counts them.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`] or [`ReadError::FieldType`] for a field that is not a `text` or `keyword` field;
    /// a corruption of a source.
    fn df(&self, field: FieldId, term: &[u8]) -> Result<u64, ReadError>;
}

/// The statistics of a snapshot's segments: `N` and the field totals summed once, `df` summed on demand and cached
/// (ROADMAP 7.2.1). Segments are immutable, so they stay valid as long as the snapshot.
#[derive(Debug)]
pub struct SegmentStatistics {
    segments: Vec<Arc<Segment>>,
    doc_count: u64,
    fields: BTreeMap<FieldId, FieldTotals>,
    df: RwLock<DfCache>,
}

#[derive(Debug, Default)]
struct DfCache {
    terms: HashMap<(FieldId, Box<[u8]>), u64>,
    bytes: usize,
}

impl SegmentStatistics {
    /// The statistics of `segments`, which must share one schema (a collection snapshot's).
    pub fn new(segments: &[Arc<Segment>]) -> Self {
        let mut doc_count = 0_u64;
        let mut fields = BTreeMap::<FieldId, FieldTotals>::new();
        for segment in segments {
            doc_count = doc_count.saturating_add(u64::from(segment.doc_count()));
            for field in segment.schema().fields() {
                if let Some(stats) = segment.field_stats(field.id()) {
                    let totals = fields.entry(field.id()).or_default();
                    *totals = totals.add(stats);
                }
            }
        }
        Self {
            segments: segments.to_vec(),
            doc_count,
            fields,
            df: RwLock::default(),
        }
    }

    fn sum_df(&self, field: FieldId, term: &[u8]) -> Result<u64, ReadError> {
        let mut df = 0_u64;
        for segment in &self.segments {
            if let Some(info) = segment.terms(field)?.get(term)? {
                df = df.saturating_add(u64::from(info.df()));
            }
        }
        Ok(df)
    }
}

impl StatisticsProvider for SegmentStatistics {
    fn doc_count(&self) -> u64 {
        self.doc_count
    }

    fn field(&self, field: FieldId) -> FieldTotals {
        self.fields.get(&field).copied().unwrap_or_default()
    }

    fn df(&self, field: FieldId, term: &[u8]) -> Result<u64, ReadError> {
        let key = (field, Box::<[u8]>::from(term));
        // A poisoned lock only means another reader panicked mid-insert; the map itself is consistent.
        if let Some(&df) = self
            .df
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .terms
            .get(&key)
        {
            return Ok(df);
        }
        let df = self.sum_df(field, term)?;
        let mut cache = self.df.write().unwrap_or_else(PoisonError::into_inner);
        let bytes = cache.bytes.saturating_add(term.len());
        if bytes <= DF_CACHE_BYTES && cache.terms.insert(key, df).is_none() {
            cache.bytes = bytes;
        }
        Ok(df)
    }
}

/// A snapshot's [`SegmentStatistics`] plus a view of its memtable: what a search over both scores with (SPEC §7.2,
/// §7.3). The memtable counts its live documents, as the segment it flushes to will.
#[derive(Debug, Clone, Copy)]
pub struct SearchStatistics<'a> {
    segments: &'a SegmentStatistics,
    memtable: &'a MemtableView,
}

impl<'a> SearchStatistics<'a> {
    /// `segments`' statistics with `memtable`'s added.
    pub fn new(segments: &'a SegmentStatistics, memtable: &'a MemtableView) -> Self {
        Self { segments, memtable }
    }
}

impl StatisticsProvider for SearchStatistics<'_> {
    fn doc_count(&self) -> u64 {
        self.segments
            .doc_count()
            .saturating_add(u64::from(self.memtable.live_count()))
    }

    fn field(&self, field: FieldId) -> FieldTotals {
        let totals = self.segments.field(field);
        match self.memtable.field_stats(field) {
            Some(stats) => totals.add(stats),
            None => totals,
        }
    }

    fn df(&self, field: FieldId, term: &[u8]) -> Result<u64, ReadError> {
        let segments = self.segments.df(field, term)?;
        // The memtable's postings hold its live documents only.
        let memtable = self
            .memtable
            .postings(field, term)?
            .map_or(0, |p| u64::from(p.df()));
        Ok(segments.saturating_add(memtable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avglen_of_an_absent_field_is_one() {
        assert_eq!(FieldTotals::default().avglen(), 1.0);
        let t = FieldTotals {
            docs_with_field: 4,
            sum_field_len: 10,
        };
        assert_eq!(t.avglen(), 2.5);
        // Corrupt totals (lengths without documents) never give a non-finite or zero mean.
        let t = FieldTotals {
            docs_with_field: 0,
            sum_field_len: 10,
        };
        assert_eq!(t.avglen(), 1.0);
        let t = FieldTotals {
            docs_with_field: 3,
            sum_field_len: 0,
        };
        assert_eq!(t.avglen(), 1.0);
    }

    #[test]
    fn totals_saturate() {
        let t = FieldTotals {
            docs_with_field: u64::MAX,
            sum_field_len: u64::MAX,
        }
        .add(FieldStats {
            docs_with_field: 1,
            sum_field_len: 1,
        });
        assert_eq!((t.docs_with_field, t.sum_field_len), (u64::MAX, u64::MAX));
    }
}

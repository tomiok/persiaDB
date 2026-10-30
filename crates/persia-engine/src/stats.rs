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
//! let stats = SegmentStatistics::new(&Arc::new(schema), &[Arc::new(segment)]);
//! assert_eq!(stats.doc_count(), 2);
//! assert_eq!(stats.field(body).avglen(), 3.5);
//! assert_eq!((stats.df(body, b"red")?, stats.df(body, b"blue")?), (2, 0));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, PoisonError, RwLock};

use crate::memtable::MemtableView;
use crate::schema::{CollectionSchema, FieldId, FieldType};
use crate::segment::{FieldStats, ReadError, Segment};
use crate::view::SearchView;

/// Approximate bytes of memory the `df` cache of a [`SegmentStatistics`] holds at most; past it, `df` is summed on
/// each call.
const DF_CACHE_BYTES: usize = 4 << 20;
/// What an entry of the cache costs besides its term's bytes: the boxed key's header, the `df`, and hash-table slack.
const DF_ENTRY_OVERHEAD: usize = 64;

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
    schema: Arc<CollectionSchema>,
    segments: Vec<Arc<Segment>>,
    doc_count: u64,
    fields: BTreeMap<FieldId, FieldTotals>,
    df: RwLock<DfCache>,
}

#[derive(Debug)]
struct DfCache {
    terms: HashMap<FieldId, HashMap<Box<[u8]>, u64>>,
    /// Approximate memory held: term bytes plus [`DF_ENTRY_OVERHEAD`] per entry.
    bytes: usize,
    cap: usize,
}

impl SegmentStatistics {
    /// The statistics of `segments`, opened with `schema` (a collection snapshot's segments and schema).
    pub fn new(schema: &Arc<CollectionSchema>, segments: &[Arc<Segment>]) -> Self {
        Self::with_cache_cap(schema, segments, DF_CACHE_BYTES)
    }

    fn with_cache_cap(
        schema: &Arc<CollectionSchema>,
        segments: &[Arc<Segment>],
        cap: usize,
    ) -> Self {
        let mut doc_count = 0_u64;
        let mut fields = BTreeMap::<FieldId, FieldTotals>::new();
        for segment in segments {
            debug_assert!(
                segment.schema().hash() == schema.hash(),
                "a segment opened with another schema"
            );
            doc_count = doc_count.saturating_add(u64::from(segment.doc_count()));
            for field in schema.fields() {
                if let Some(stats) = segment.field_stats(field.id()) {
                    let totals = fields.entry(field.id()).or_default();
                    *totals = totals.add(stats);
                }
            }
        }
        Self {
            schema: Arc::clone(schema),
            segments: segments.to_vec(),
            doc_count,
            fields,
            df: RwLock::new(DfCache {
                terms: HashMap::new(),
                bytes: 0,
                cap,
            }),
        }
    }

    /// Checks `field` is a `text` or `keyword` field of the schema, as a segment's `terms` does, so the answer does
    /// not depend on how many segments there are.
    fn check_field(&self, field: FieldId) -> Result<(), ReadError> {
        let f = self
            .schema
            .field_by_id(field)
            .ok_or(ReadError::UnknownField { field })?;
        match f.field_type() {
            FieldType::Text | FieldType::Keyword => Ok(()),
            field_type => Err(ReadError::FieldType {
                field,
                field_type,
                what: "terms",
            }),
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
        self.check_field(field)?;
        // A poisoned lock only means another reader panicked mid-insert; the map itself is consistent.
        let cached = self
            .df
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .terms
            .get(&field)
            .and_then(|terms| terms.get(term).copied());
        if let Some(df) = cached {
            return Ok(df);
        }
        let df = self.sum_df(field, term)?;
        let mut cache = self.df.write().unwrap_or_else(PoisonError::into_inner);
        let bytes = cache
            .bytes
            .saturating_add(term.len())
            .saturating_add(DF_ENTRY_OVERHEAD);
        if bytes <= cache.cap
            && cache
                .terms
                .entry(field)
                .or_default()
                .insert(term.into(), df)
                .is_none()
        {
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
        // Its live documents only, as its postings.
        let memtable = u64::from(self.memtable.df(field, term)?);
        Ok(segments.saturating_add(memtable))
    }
}

#[cfg(test)]
mod tests {
    use persia_analysis::AnalyzerRegistry;
    use persia_format::ByteSource;

    use super::*;
    use crate::document::{Document, Value};
    use crate::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
    use crate::segment::{SegmentBuilder, SegmentMeta, SegmentWriter};

    fn collection() -> (Arc<CollectionSchema>, Arc<Segment>) {
        let schema = SchemaBuilder::new("c")
            .field("body", FieldKind::Text(TextOptions::default()))
            .field("tag", FieldKind::Keyword(KeywordOptions::default()))
            .field("n", FieldKind::I64 { stored: false })
            .build()
            .unwrap();
        let body = schema.field("body").unwrap().id();
        let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        for (id, text) in [("a", "aa bb"), ("b", "bb cc"), ("c", "cc dd")] {
            b.add(&Document::new(id).with(body, Value::String(text.into())))
                .unwrap();
        }
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
        let segment = Segment::open(ByteSource::new(bytes.into()), &schema).unwrap();
        (Arc::new(schema), Arc::new(segment))
    }

    #[test]
    fn the_cache_stops_at_its_cap_and_answers_stay_right() {
        let (schema, segment) = collection();
        let body = schema.field("body").unwrap().id();
        // Room for two entries of two-byte terms.
        let cap = 2 * (2 + DF_ENTRY_OVERHEAD);
        let stats = SegmentStatistics::with_cache_cap(&schema, &[segment], cap);
        let expected = [("aa", 1), ("bb", 2), ("cc", 2), ("dd", 1), ("ee", 0)];
        for _ in 0..2 {
            for (term, df) in expected {
                assert_eq!(stats.df(body, term.as_bytes()).unwrap(), df, "{term}");
            }
        }
        let cache = stats.df.read().unwrap();
        let entries: usize = cache.terms.values().map(HashMap::len).sum();
        assert_eq!((entries, cache.bytes), (2, cap));
    }

    #[test]
    fn df_checks_the_field_with_or_without_segments() {
        let (schema, segment) = collection();
        let id = |n| schema.field(n).unwrap().id();
        for segments in [vec![], vec![segment]] {
            let stats = SegmentStatistics::new(&schema, &segments);
            assert!(matches!(
                stats.df(id("n"), b"x"),
                Err(ReadError::FieldType { .. })
            ));
            assert!(matches!(
                stats.df(FieldId::new(9), b"x"),
                Err(ReadError::UnknownField { .. })
            ));
            assert_eq!(stats.df(id("tag"), b"x").unwrap(), 0);
        }
    }

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

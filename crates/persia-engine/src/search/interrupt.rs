//! A view whose reads look at a search's interrupts (SPEC §7.3, ROADMAP 7.6.3): its postings, term streams, column
//! scans and column readers read the deadline and the cancellation token every [`READ_EVERY`] steps, and fail with
//! [`ReadError::Interrupted`] once either says stop.
//!
//! A collector looks between the matches it is yielded; the walks between two matches (a phrase over common terms
//! that never line up, a pruning disjunction skipping blocks, a conjunction leapfrogging), a filter's scan, a facet's
//! count and a `prefix` expansion all advance one of these readers, so they stop too.

use core::cell::Cell;
use core::ops::Bound;

use fst::Automaton;
use roaring::RoaringBitmap;

use super::Interrupt;
use crate::document::Document;
use crate::schema::FieldId;
use crate::score::FieldHit;
use crate::segment::{BlockMax, FieldStats, ReadError};
use crate::view::{KeyReader, KeywordReader, PostingsIterator, SearchView, TermEnum};

/// How many steps a reader takes between two looks at its interrupts: a posting moved to, a block skipped, a term,
/// a column key or a document's values read. A look reads a clock; a step costs a few nanoseconds.
pub(crate) const READ_EVERY: u32 = 1024;

/// What stops a search: a cancelled token or a passed deadline.
pub(crate) trait Interrupts: Sync {
    /// The interrupt in force, if any.
    ///
    /// # Errors
    /// The search was cancelled or ran past its deadline.
    fn check(&self) -> Result<(), Interrupt>;
}

/// `view`, its reads counted in `steps`.
pub(crate) struct Interruptible<'s, V> {
    view: &'s V,
    steps: &'s Steps<'s>,
}

impl<'s, V> Interruptible<'s, V> {
    pub(crate) fn new(view: &'s V, steps: &'s Steps<'s>) -> Self {
        Self { view, steps }
    }
}

/// One count of the steps of every reader of a view, or of several views walked in turn (an expansion over a
/// collection's segments): the interrupts are read every [`READ_EVERY`] steps of them all, however many readers
/// share the walk.
pub(crate) struct Steps<'s> {
    stop: &'s dyn Interrupts,
    count: Cell<u32>,
}

impl<'s> Steps<'s> {
    pub(crate) fn new(stop: &'s dyn Interrupts) -> Self {
        Self {
            stop,
            count: Cell::new(0),
        }
    }

    /// One step: every [`READ_EVERY`]th looks at the interrupts.
    #[inline]
    fn step(&self) -> Result<(), ReadError> {
        let n = self.count.get() + 1;
        if n < READ_EVERY {
            self.count.set(n);
            return Ok(());
        }
        self.count.set(0);
        self.stop.check().map_err(ReadError::Interrupted)
    }
}

/// A reader of an [`Interruptible`] view.
pub(crate) struct Watched<'s, R> {
    inner: R,
    steps: &'s Steps<'s>,
}

impl<'s, R> Watched<'s, R> {
    fn new(inner: R, steps: &'s Steps<'s>) -> Self {
        Self { inner, steps }
    }

    #[inline]
    fn step(&self) -> Result<(), ReadError> {
        self.steps.step()
    }
}

impl<P: PostingsIterator> PostingsIterator for Watched<'_, P> {
    fn df(&self) -> u32 {
        self.inner.df()
    }

    fn doc(&self) -> Option<u32> {
        self.inner.doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        self.step()?;
        self.inner.next()
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        self.step()?;
        self.inner.advance(target)
    }

    fn freq(&mut self) -> Result<u32, ReadError> {
        self.inner.freq()
    }

    fn positions(&mut self) -> Result<&[u32], ReadError> {
        self.inner.positions()
    }

    fn block_max(&self) -> Option<BlockMax> {
        self.inner.block_max()
    }

    fn shallow_advance(&mut self, target: u32) -> Result<Option<BlockMax>, ReadError> {
        self.step()?;
        self.inner.shallow_advance(target)
    }

    fn term_bound(&self) -> FieldHit {
        self.inner.term_bound()
    }
}

impl<T: TermEnum> TermEnum for Watched<'_, T> {
    fn next(&mut self) -> Result<Option<(&[u8], u32)>, ReadError> {
        self.step()?;
        self.inner.next()
    }
}

impl<K: KeyReader> KeyReader for Watched<'_, K> {
    fn key(&mut self, docid: u32) -> Result<Option<u64>, ReadError> {
        self.step()?;
        self.inner.key(docid)
    }
}

impl<K: KeywordReader> KeywordReader for Watched<'_, K> {
    fn ords(&mut self, docid: u32) -> Result<&[u64], ReadError> {
        self.step()?;
        self.inner.ords(docid)
    }

    fn value(&self, ord: u64) -> Option<&[u8]> {
        self.inner.value(ord)
    }

    fn len(&self) -> u64 {
        self.inner.len()
    }
}

impl<V: SearchView> SearchView for Interruptible<'_, V> {
    type Norms<'a>
        = V::Norms<'a>
    where
        Self: 'a;
    type Postings<'a>
        = Watched<'a, V::Postings<'a>>
    where
        Self: 'a;
    type Terms<'a, A>
        = Watched<'a, V::Terms<'a, A>>
    where
        Self: 'a,
        A: Automaton + 'a;
    type Keys<'a>
        = Watched<'a, V::Keys<'a>>
    where
        Self: 'a;
    type Keywords<'a>
        = Watched<'a, V::Keywords<'a>>
    where
        Self: 'a;

    fn doc_count(&self) -> u32 {
        self.view.doc_count()
    }

    fn is_live(&self, docid: u32) -> bool {
        self.view.is_live(docid)
    }

    fn next_live(&self, from: u32) -> Result<Option<u32>, ReadError> {
        for docid in from..self.view.doc_count() {
            self.steps.step()?;
            if self.view.is_live(docid) {
                return Ok(Some(docid));
            }
        }
        Ok(None)
    }

    fn field_stats(&self, field: FieldId) -> Option<FieldStats> {
        self.view.field_stats(field)
    }

    fn postings(
        &self,
        field: FieldId,
        term: &[u8],
    ) -> Result<Option<Self::Postings<'_>>, ReadError> {
        Ok(self
            .view
            .postings(field, term)?
            .map(|p| Watched::new(p, self.steps)))
    }

    fn terms<'a, A: Automaton + 'a>(
        &'a self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        automaton: A,
    ) -> Result<Self::Terms<'a, A>, ReadError> {
        let terms = self
            .view
            .terms_stepped(field, lower, upper, automaton, &|| self.steps.step())?;
        Ok(Watched::new(terms, self.steps))
    }

    fn deleted(&self) -> RoaringBitmap {
        self.view.deleted()
    }

    fn present(&self, field: FieldId) -> Result<RoaringBitmap, ReadError> {
        self.view.present(field)
    }

    fn trues(&self, field: FieldId) -> Result<RoaringBitmap, ReadError> {
        self.view.trues(field)
    }

    fn try_column_keys(
        &self,
        field: FieldId,
        mut f: impl FnMut(u32, u64) -> Result<(), ReadError>,
    ) -> Result<(), ReadError> {
        self.view.try_column_keys(field, |docid, key| {
            self.steps.step()?;
            f(docid, key)
        })
    }

    fn norms(&self, field: FieldId) -> Result<Self::Norms<'_>, ReadError> {
        self.view.norms(field)
    }

    fn keys(&self, field: FieldId) -> Result<Self::Keys<'_>, ReadError> {
        Ok(Watched::new(self.view.keys(field)?, self.steps))
    }

    fn keywords(&self, field: FieldId) -> Result<Self::Keywords<'_>, ReadError> {
        Ok(Watched::new(self.view.keywords(field)?, self.steps))
    }

    fn document(&self, docid: u32) -> Result<Document, ReadError> {
        self.view.document(docid)
    }

    fn docid_of(&self, id: &str) -> Result<Option<u32>, ReadError> {
        self.view.docid_of(id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use persia_analysis::AnalyzerRegistry;
    use persia_format::ByteSource;

    use super::*;
    use crate::document::Value;
    use crate::query::{
        BoolQuery, FilterValue, PhraseQuery, PhraseTerm, PrefixQuery, Query, TermQuery,
    };
    use crate::schema::{CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
    use crate::search::{FacetCounter, FacetRequest, SearchError, SearchPlan};
    use crate::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    use crate::stats::SegmentStatistics;
    use crate::view::SegmentView;

    const DOCS: u32 = 3_000;

    /// Stops at its `after`th look, and counts them.
    struct StopAfter {
        after: u32,
        looks: AtomicU32,
    }

    impl StopAfter {
        fn new(after: u32) -> Self {
            Self {
                after,
                looks: AtomicU32::new(0),
            }
        }

        fn looks(&self) -> u32 {
            self.looks.load(Ordering::Relaxed)
        }
    }

    impl Interrupts for StopAfter {
        fn check(&self) -> Result<(), Interrupt> {
            let n = self.looks.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= self.after {
                return Err(Interrupt::DeadlineExceeded);
            }
            Ok(())
        }
    }

    struct Fixture {
        schema: Arc<CollectionSchema>,
        segment: Arc<Segment>,
        body: FieldId,
        tag: FieldId,
        n: FieldId,
    }

    /// `DOCS` documents "alpha x beta": the phrase "alpha beta" never lines up. Each has its own tag and `n`.
    fn fixture() -> Fixture {
        let schema = SchemaBuilder::new("c")
            .field(
                "body",
                FieldKind::Text(TextOptions {
                    positions: true,
                    ..TextOptions::default()
                }),
            )
            .field(
                "tag",
                FieldKind::Keyword(KeywordOptions {
                    facet: true,
                    ..KeywordOptions::default()
                }),
            )
            .field("n", FieldKind::I64 { stored: false })
            .build()
            .unwrap();
        let body = schema.field("body").unwrap().id();
        let tag = schema.field("tag").unwrap().id();
        let n = schema.field("n").unwrap().id();
        let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        for i in 0..DOCS {
            let doc = Document::new(format!("d{i}"))
                .with(body, Value::String("alpha x beta".into()))
                .with(tag, Value::String(format!("t{i:05}")))
                .with(n, Value::I64(i64::from(i)));
            b.add(&doc).unwrap();
        }
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
        let segment = Arc::new(Segment::open(ByteSource::new(bytes.into()), &schema).unwrap());
        Fixture {
            schema: Arc::new(schema),
            segment,
            body,
            tag,
            n,
        }
    }

    fn interrupted<T: core::fmt::Debug>(r: &Result<T, ReadError>) {
        assert!(
            matches!(r, Err(ReadError::Interrupted(Interrupt::DeadlineExceeded))),
            "{r:?}"
        );
    }

    #[test]
    fn a_phrase_that_never_lines_up_stops_between_matches() {
        let f = fixture();
        let stats = SegmentStatistics::new(&f.schema, &[Arc::clone(&f.segment)]);
        let view = SegmentView::new(&f.segment, None);
        let phrase = Query::Phrase(PhraseQuery {
            field: f.body,
            terms: ["alpha", "beta"]
                .into_iter()
                .zip(0..)
                .map(|(t, position)| PhraseTerm {
                    term: t.into(),
                    position,
                })
                .collect(),
        });
        let plan = SearchPlan::new(&phrase, &f.schema, &stats).unwrap();
        // No match at all: a collector would never look.
        assert_eq!(plan.top_k(&view, 10).unwrap(), []);
        let stop = StopAfter::new(1);
        interrupted(&plan.top_k(&Interruptible::new(&view, &Steps::new(&stop)), 10));
        // A stop that never comes changes nothing; 3 000 candidates read twice each look a few times.
        let never = StopAfter::new(u32::MAX);
        assert_eq!(
            plan.top_k(&Interruptible::new(&view, &Steps::new(&never)), 10)
                .unwrap(),
            []
        );
        assert!(never.looks() >= 2, "{}", never.looks());
    }

    #[test]
    fn a_conjunction_without_a_common_document_stops() {
        let f = fixture();
        let stats = SegmentStatistics::new(&f.schema, &[Arc::clone(&f.segment)]);
        let view = SegmentView::new(&f.segment, None);
        // `alpha AND NOT alpha`: every candidate is excluded, so nothing is yielded.
        let alpha = Query::Term(TermQuery {
            field: f.body,
            term: "alpha".into(),
        });
        let query = Query::Bool(BoolQuery {
            must: vec![alpha.clone()],
            must_not: vec![alpha],
            ..BoolQuery::default()
        });
        let plan = SearchPlan::new(&query, &f.schema, &stats).unwrap();
        assert_eq!(plan.top_k(&view, 10).unwrap(), []);
        interrupted(&plan.top_k(
            &Interruptible::new(&view, &Steps::new(&StopAfter::new(1))),
            10,
        ));
    }

    #[test]
    fn a_range_filter_stops_while_the_matcher_is_built() {
        let f = fixture();
        let stats = SegmentStatistics::new(&f.schema, &[Arc::clone(&f.segment)]);
        let view = SegmentView::new(&f.segment, None);
        let query = Query::Bool(BoolQuery {
            filter: vec![crate::query::Filter::Range {
                field: f.n,
                lower: Bound::Included(FilterValue::I64(10)),
                upper: Bound::Unbounded,
            }],
            ..BoolQuery::default()
        });
        let plan = SearchPlan::new(&query, &f.schema, &stats).unwrap();
        assert!(plan.matcher(&view).is_ok());
        interrupted(
            &plan
                .matcher(&Interruptible::new(&view, &Steps::new(&StopAfter::new(1))))
                .map(|_| ()),
        );
    }

    #[test]
    fn a_facet_stops_while_it_counts() {
        let f = fixture();
        let view = SegmentView::new(&f.segment, None);
        let docs = crate::bitmap::doc_range(DOCS).unwrap();
        let request = FacetRequest::new(f.tag, 10, &f.schema).unwrap();
        let mut facet = FacetCounter::new(request);
        assert!(facet.add(&view, &docs).is_ok());
        let mut facet = FacetCounter::new(request);
        interrupted(&facet.add(
            &Interruptible::new(&view, &Steps::new(&StopAfter::new(1))),
            &docs,
        ));
    }

    #[test]
    fn a_prefix_stops_while_it_expands() {
        let f = fixture();
        let stats = SegmentStatistics::new(&f.schema, &[Arc::clone(&f.segment)]);
        let view = SegmentView::new(&f.segment, None);
        let prefix = Query::Prefix(PrefixQuery {
            field: f.tag,
            prefix: "t".into(),
            max_expansions: 50,
        });
        assert!(SearchPlan::with_terms(&prefix, &f.schema, &stats, &view).is_ok());
        let stop = StopAfter::new(1);
        let planned = SearchPlan::with_terms(
            &prefix,
            &f.schema,
            &stats,
            &Interruptible::new(&view, &Steps::new(&stop)),
        );
        assert!(
            matches!(
                planned,
                Err(SearchError::Interrupted(Interrupt::DeadlineExceeded))
            ),
            "{planned:?}"
        );
    }

    #[test]
    fn a_view_looks_every_read_every_steps() {
        let never = StopAfter::new(u32::MAX);
        let steps = Steps::new(&never);
        for _ in 0..READ_EVERY * 3 - 1 {
            steps.step().unwrap();
        }
        assert_eq!(never.looks(), 2);
        steps.step().unwrap();
        assert_eq!(never.looks(), 3);
    }

    #[test]
    fn many_short_readers_share_one_count() {
        let f = fixture();
        let stats = SegmentStatistics::new(&f.schema, &[Arc::clone(&f.segment)]);
        let view = SegmentView::new(&f.segment, None);
        // 3 000 terms of one document each: no reader takes 1 024 steps, the view's readers together do.
        let prefix = Query::Prefix(PrefixQuery {
            field: f.tag,
            prefix: "t".into(),
            max_expansions: DOCS,
        });
        let plan = SearchPlan::with_terms(&prefix, &f.schema, &stats, &view).unwrap();
        let never = StopAfter::new(u32::MAX);
        let hits = plan
            .top_k_with(
                &Interruptible::new(&view, &Steps::new(&never)),
                10,
                crate::search::Pruning::Exhaustive,
            )
            .unwrap();
        assert_eq!(hits.len(), 10);
        assert!(never.looks() >= 2, "{}", never.looks());
    }

    /// A memtable of `DOCS` documents shaped as [`fixture`]'s, and a view of its first `seen`.
    fn memtable(seen: u32) -> (crate::memtable::MemtableView, Fixture) {
        let f = fixture();
        let mut mem =
            crate::memtable::Memtable::new(&f.schema, &AnalyzerRegistry::builtin()).unwrap();
        let doc = |i: u32| {
            Document::new(format!("d{i}"))
                .with(f.body, Value::String("alpha x beta".into()))
                .with(f.tag, Value::String(format!("t{i:05}")))
                .with(f.n, Value::I64(i64::from(i)))
        };
        for i in 0..seen {
            mem.put(&doc(i)).unwrap();
        }
        let view = mem.view();
        // Written after the view: none of it is in it.
        for i in seen..DOCS {
            mem.put(&doc(i)).unwrap();
        }
        (view, f)
    }

    #[test]
    fn a_memtable_copies_its_terms_and_keys_in_chunks_up_to_its_view() {
        // 2 500 seen: chunks end at 1 024 and 2 048, and the view's cutoff falls inside the third.
        let (view, f) = memtable(2_500);
        let mut terms = view
            .terms(
                f.tag,
                Bound::Unbounded,
                Bound::Unbounded,
                fst::automaton::Str::new("t").starts_with(),
            )
            .unwrap();
        let mut listed = Vec::new();
        while let Some((term, df)) = terms.next().unwrap() {
            assert_eq!(df, 1);
            listed.push(String::from_utf8(term.to_vec()).unwrap());
        }
        let expected: Vec<String> = (0..2_500).map(|i| format!("t{i:05}")).collect();
        assert_eq!(listed, expected);
        let mut keys = Vec::new();
        view.column_keys(f.n, |docid, key| keys.push((docid, key)))
            .unwrap();
        assert_eq!(keys.len(), 2_500);
        for (i, &(docid, key)) in (0_u32..).zip(&keys) {
            assert_eq!(docid, i);
            assert_eq!(key, persia_format::for_codec::i64_to_key(i64::from(i)));
        }
    }

    #[test]
    fn a_memtable_expansion_lists_its_view_while_writers_interleave() {
        let f = fixture();
        let mem = std::sync::Mutex::new(
            crate::memtable::Memtable::new(&f.schema, &AnalyzerRegistry::builtin()).unwrap(),
        );
        let doc = |id: String, tag: String| {
            Document::new(id)
                .with(f.body, Value::String("alpha".into()))
                .with(f.tag, Value::String(tag))
        };
        for i in 0..2_500 {
            mem.lock()
                .unwrap()
                .put(&doc(format!("d{i}"), format!("t{i:05}")))
                .unwrap();
        }
        let view = mem.lock().unwrap().view();
        // Between the first chunks: a put whose log fails (its new tag is truncated from the arena), new tags, a
        // replaced document with an old tag, a delete, all after the view.
        let steps = Cell::new(0_u32);
        let step = || {
            steps.set(steps.get() + 1);
            if steps.get() == 1_024 {
                let mut mem = mem.lock().unwrap();
                let failed = mem.put_logged(&doc("x".into(), "t99999".into()), |_| Err("disk"));
                assert!(failed.is_err());
                for i in 0..3_000 {
                    mem.put(&doc(format!("n{i}"), format!("t{:05}", 50_000 + i)))
                        .unwrap();
                }
                mem.put(&doc("d7".into(), "t00008".into())).unwrap();
                mem.delete("d9").unwrap();
            }
            Ok(())
        };
        let mut terms = view
            .terms_stepped(
                f.tag,
                Bound::Unbounded,
                Bound::Unbounded,
                fst::automaton::Str::new("t").starts_with(),
                &step,
            )
            .unwrap();
        let mut listed = Vec::new();
        while let Some((term, df)) = terms.next().unwrap() {
            listed.push((String::from_utf8(term.to_vec()).unwrap(), df));
        }
        let expected: Vec<(String, u32)> = (0..2_500).map(|i| (format!("t{i:05}"), 1)).collect();
        assert_eq!(listed, expected);
        // The 3 000 tags interned after the view were not tried: one step per term of the view, then per match.
        assert_eq!(steps.get(), 2 * 2_500);
    }

    #[test]
    fn a_memtable_column_copy_ends_on_a_chunk_boundary() {
        let (view, f) = memtable(2_048);
        let mut keys = Vec::new();
        view.column_keys(f.n, |docid, _| keys.push(docid)).unwrap();
        assert_eq!(keys, (0..2_048).collect::<Vec<_>>());
    }
    #[test]
    fn a_memtable_expansion_that_matches_nothing_stops() {
        let (view, f) = memtable(DOCS);
        let stats = SegmentStatistics::new(&f.schema, &[]);
        let prefix = Query::Prefix(PrefixQuery {
            field: f.tag,
            prefix: "zz".into(),
            max_expansions: 50,
        });
        assert!(SearchPlan::with_terms(&prefix, &f.schema, &stats, &view).is_ok());
        let planned = SearchPlan::with_terms(
            &prefix,
            &f.schema,
            &stats,
            &Interruptible::new(&view, &Steps::new(&StopAfter::new(1))),
        );
        assert!(
            matches!(
                planned,
                Err(SearchError::Interrupted(Interrupt::DeadlineExceeded))
            ),
            "{planned:?}"
        );
    }

    #[test]
    fn a_memtable_range_filter_stops() {
        let (view, f) = memtable(DOCS);
        let stats = SegmentStatistics::new(&f.schema, &[]);
        let query = Query::Bool(BoolQuery {
            filter: vec![crate::query::Filter::Range {
                field: f.n,
                lower: Bound::Included(FilterValue::I64(i64::MAX)),
                upper: Bound::Unbounded,
            }],
            ..BoolQuery::default()
        });
        let plan = SearchPlan::new(&query, &f.schema, &stats).unwrap();
        assert_eq!(plan.top_k(&view, 10).unwrap(), []);
        interrupted(
            &plan
                .matcher(&Interruptible::new(&view, &Steps::new(&StopAfter::new(1))))
                .map(|_| ()),
        );
    }
    #[test]
    fn match_all_over_deleted_documents_stops() {
        let f = fixture();
        let stats = SegmentStatistics::new(&f.schema, &[Arc::clone(&f.segment)]);
        // Every document deleted but the last: `match_all` walks 2 999 docids to its first.
        let deleted: RoaringBitmap = (0..DOCS - 1).collect();
        let tombstones = f.segment.tombstones(deleted).unwrap();
        let view = SegmentView::new(&f.segment, Some(&tombstones));
        let plan = SearchPlan::new(&Query::MatchAll, &f.schema, &stats).unwrap();
        assert_eq!(plan.top_k(&view, 10).unwrap().len(), 1);
        interrupted(&plan.top_k(
            &Interruptible::new(&view, &Steps::new(&StopAfter::new(1))),
            10,
        ));
    }
}

//! A view whose reads look at a search's interrupts (SPEC §7.3, ROADMAP 7.6.3): its postings, term streams, column
//! scans and column readers read the deadline and the cancellation token every [`READ_EVERY`] steps, and fail with
//! [`ReadError::Interrupted`] once either says stop.
//!
//! A collector looks between the matches it is yielded; the walks between two matches (a phrase over common terms
//! that never line up, a pruning disjunction skipping blocks, a conjunction leapfrogging), a filter's scan, a facet's
//! count and a `prefix` expansion all advance one of these readers, so they stop too.

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

/// `view`, its reads stopped by `stop`.
pub(crate) struct Interruptible<'s, V> {
    view: &'s V,
    stop: &'s dyn Interrupts,
}

impl<'s, V> Interruptible<'s, V> {
    pub(crate) fn new(view: &'s V, stop: &'s dyn Interrupts) -> Self {
        Self { view, stop }
    }
}

/// A reader of an [`Interruptible`] view.
pub(crate) struct Watched<'s, R> {
    inner: R,
    stop: &'s dyn Interrupts,
    steps: u32,
}

impl<'s, R> Watched<'s, R> {
    fn new(inner: R, stop: &'s dyn Interrupts) -> Self {
        Self {
            inner,
            stop,
            steps: 0,
        }
    }

    /// One step: every [`READ_EVERY`]th looks at the interrupts.
    #[inline]
    fn step(&mut self) -> Result<(), ReadError> {
        step(&mut self.steps, self.stop)
    }
}

#[inline]
fn step(steps: &mut u32, stop: &dyn Interrupts) -> Result<(), ReadError> {
    *steps += 1;
    if *steps < READ_EVERY {
        return Ok(());
    }
    *steps = 0;
    stop.check().map_err(ReadError::Interrupted)
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
            .map(|p| Watched::new(p, self.stop)))
    }

    fn terms<'a, A: Automaton + 'a>(
        &'a self,
        field: FieldId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        automaton: A,
    ) -> Result<Self::Terms<'a, A>, ReadError> {
        let terms = self.view.terms(field, lower, upper, automaton)?;
        Ok(Watched::new(terms, self.stop))
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
        let mut steps = 0;
        self.view.try_column_keys(field, |docid, key| {
            step(&mut steps, self.stop)?;
            f(docid, key)
        })
    }

    fn norms(&self, field: FieldId) -> Result<Self::Norms<'_>, ReadError> {
        self.view.norms(field)
    }

    fn keys(&self, field: FieldId) -> Result<Self::Keys<'_>, ReadError> {
        Ok(Watched::new(self.view.keys(field)?, self.stop))
    }

    fn keywords(&self, field: FieldId) -> Result<Self::Keywords<'_>, ReadError> {
        Ok(Watched::new(self.view.keywords(field)?, self.stop))
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
        interrupted(&plan.top_k(&Interruptible::new(&view, &stop), 10));
        // A stop that never comes changes nothing; 3 000 candidates read twice each look a few times.
        let never = StopAfter::new(u32::MAX);
        assert_eq!(
            plan.top_k(&Interruptible::new(&view, &never), 10).unwrap(),
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
        interrupted(&plan.top_k(&Interruptible::new(&view, &StopAfter::new(1)), 10));
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
                .matcher(&Interruptible::new(&view, &StopAfter::new(1)))
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
        interrupted(&facet.add(&Interruptible::new(&view, &StopAfter::new(1)), &docs));
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
            &Interruptible::new(&view, &stop),
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
    fn a_reader_looks_every_read_every_steps() {
        let never = StopAfter::new(u32::MAX);
        let mut steps = 0;
        for _ in 0..READ_EVERY * 3 - 1 {
            step(&mut steps, &never).unwrap();
        }
        assert_eq!(never.looks(), 2);
        step(&mut steps, &never).unwrap();
        assert_eq!(never.looks(), 3);
    }
}

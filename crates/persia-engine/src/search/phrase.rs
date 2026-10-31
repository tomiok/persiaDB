//! Phrase matching with positions (ROADMAP 7.4.2): the documents holding every term of a phrase, leapfrogged as a
//! conjunction, where the terms also stand at their relative positions; scored from the phrase frequency (SPEC §7.2).

use crate::schema::FieldId;
use crate::score::{FieldHit, TermScorer};
use crate::segment::ReadError;
use crate::view::{PostingsIterator, SearchView};

use super::boolean::Conjunction;
use super::{Matcher, Pos, bounded_norm};

/// The postings of one phrase term as a [`Matcher`] (it scores nothing), with its relative position.
pub(super) struct PhrasePostings<P> {
    pub(super) postings: P,
    pub(super) position: u32,
    /// Its positions in the current candidate, copied from `postings`.
    buffer: Vec<u32>,
}

impl<P: PostingsIterator> PhrasePostings<P> {
    pub(super) fn new(postings: P, position: u32) -> Self {
        Self {
            postings,
            position,
            buffer: Vec::new(),
        }
    }
}

impl<P: PostingsIterator> Matcher for PhrasePostings<P> {
    fn cost(&self) -> u64 {
        u64::from(self.postings.df())
    }

    fn doc(&self) -> Option<u32> {
        self.postings.doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        self.postings.next()
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        self.postings.advance(target)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        Ok(0.0)
    }

    fn max_score(&self) -> f64 {
        0.0
    }
}

/// The documents holding a phrase, at least one term.
pub(super) struct PhraseMatcher<'a, V: SearchView + 'a> {
    terms: Conjunction<PhrasePostings<V::Postings<'a>>>,
    field: FieldId,
    norms: V::Norms<'a>,
    scorer: &'a TermScorer,
    /// The phrase frequency of the current document, at least 1.
    freq: u32,
    pos: Pos,
    /// [`Matcher::max_score`].
    max: f64,
}

impl<'a, V: SearchView + 'a> PhraseMatcher<'a, V> {
    pub(super) fn new(
        terms: Vec<PhrasePostings<V::Postings<'a>>>,
        field: FieldId,
        norms: V::Norms<'a>,
        scorer: &'a TermScorer,
    ) -> Self {
        // A start counts once and maps to one position of each term, so the phrase frequency is at most every
        // term's frequency; a document holding every term has a norm at least each term's smallest.
        let bound = FieldHit {
            tf: terms
                .iter()
                .map(|t| t.postings.term_bound().tf)
                .min()
                .unwrap_or(0),
            norm: terms
                .iter()
                .map(|t| t.postings.term_bound().norm)
                .max()
                .unwrap_or(0),
        };
        Self {
            max: scorer.score(&[Some(bound)]),
            terms: Conjunction::new(terms),
            field,
            norms,
            scorer,
            freq: 0,
            pos: Pos::Unstarted,
        }
    }

    /// From `candidate` on, the first document of the conjunction that holds the phrase.
    fn find(&mut self, mut candidate: Option<u32>) -> Result<Option<u32>, ReadError> {
        while candidate.is_some() {
            self.freq = phrase_freq(self.terms.clauses_mut())?;
            if self.freq > 0 {
                return Ok(candidate);
            }
            candidate = self.terms.next()?;
        }
        Ok(None)
    }
}

impl<'a, V: SearchView + 'a> Matcher for PhraseMatcher<'a, V> {
    fn cost(&self) -> u64 {
        self.terms.cost()
    }

    fn doc(&self) -> Option<u32> {
        self.pos.doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        let Some(target) = self.pos.next_target() else {
            self.pos = Pos::Done;
            return Ok(None);
        };
        self.advance(target)
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        match self.pos {
            Pos::At(d) if d >= target => return Ok(Some(d)),
            Pos::Done => return Ok(None),
            Pos::Unstarted | Pos::At(_) => {}
        }
        self.pos = Pos::Done;
        let candidate = self.terms.advance(target)?;
        let doc = self.find(candidate)?;
        self.pos = Pos::of(doc);
        Ok(doc)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        let Some(doc) = self.pos.doc() else {
            return Ok(0.0);
        };
        // Each term is on `doc`: its block's smallest norm bounds the document's.
        let min_norm = self
            .terms
            .clauses_mut()
            .iter()
            .filter_map(|t| t.postings.block_max())
            .map(|b| b.min_norm)
            .max();
        let hit = FieldHit {
            tf: self.freq,
            norm: bounded_norm(&mut self.norms, self.field, doc, min_norm)?,
        };
        Ok(self.scorer.score(&[Some(hit)]))
    }

    fn max_score(&self) -> f64 {
        self.max
    }
}

/// The phrase frequency of the document every term is on: how many distinct starts `s` have each term at
/// `s + position` (SPEC §7.2). Starts below 0 or past `u32::MAX` hold nothing.
fn phrase_freq<P: PostingsIterator>(terms: &mut [PhrasePostings<P>]) -> Result<u32, ReadError> {
    for term in terms.iter_mut() {
        let positions = term.postings.positions()?;
        term.buffer.clear();
        term.buffer.extend_from_slice(positions);
    }
    // Starts come from the term with the fewest positions; the others are checked by binary search.
    let Some(anchor) = (0..terms.len()).min_by_key(|&i| terms.get(i).map_or(0, |t| t.buffer.len()))
    else {
        return Ok(0);
    };
    let Some(anchor) = terms.get(anchor) else {
        return Ok(0);
    };
    let mut freq = 0_u32;
    let mut last = None;
    for &p in &anchor.buffer {
        let Some(start) = p.checked_sub(anchor.position) else {
            continue;
        };
        // Stacked tokens repeat a position: count each start once.
        if last == Some(start) {
            continue;
        }
        last = Some(start);
        let all = terms.iter().all(|t| {
            start
                .checked_add(t.position)
                .is_some_and(|at| t.buffer.binary_search(&at).is_ok())
        });
        if all {
            freq = freq.saturating_add(1);
        }
    }
    Ok(freq)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use persia_analysis::{
        Analyzer, AnalyzerRegistry, TokenFilter, TokenStream, WhitespaceTokenizer,
    };
    use persia_format::ByteSource;

    use crate::document::{Document, Value};
    use crate::query::{PhraseQuery, PhraseTerm, Query, TermQuery};
    use crate::schema::{
        AnalyzerRef, CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions,
    };
    use crate::score::{FieldHit, TermScorer};
    use crate::search::SearchPlan;
    use crate::segment::builder::FieldBuilder;
    use crate::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    use crate::stats::SegmentStatistics;
    use crate::view::{NormReader, SearchView, SegmentView};

    /// Index-time synonyms stacked on the source token's position (SPEC §6.3.4).
    ///
    /// Each token is also emitted again as itself, so a term's postings repeat that position: the case SPEC §7.2
    /// names, where a phrase start still counts once. `bee` additionally stacks the synonym `insect`. No built-in
    /// analyzer does this, and the reference keeps one token per position, so the test swaps this filter in.
    struct Synonyms;

    impl TokenFilter for Synonyms {
        fn apply(&self, tokens: &mut TokenStream) {
            tokens.rebuild(|token, out| {
                let (start, end, position) = (token.start, token.end, token.position);
                out.push(&token.text, start, end, position);
                if token.text == "bee" {
                    out.push("insect", start, end, position);
                }
                out.push_token(token);
            });
        }

        fn stacks_tokens(&self) -> bool {
            true
        }
    }

    fn phrase(field: FieldId, terms: &[(&str, u32)]) -> PhraseQuery {
        PhraseQuery {
            field,
            terms: terms
                .iter()
                .map(|&(term, position)| PhraseTerm {
                    term: term.into(),
                    position,
                })
                .collect(),
        }
    }

    fn scores(plan: &SearchPlan, view: &SegmentView<'_>) -> Vec<(u32, f64)> {
        let mut hits: Vec<_> = plan
            .top_k(view, 10)
            .unwrap()
            .into_iter()
            .map(|hit| (hit.docid, hit.score))
            .collect();
        hits.sort_by_key(|hit| hit.0);
        hits
    }

    /// The score a phrase of frequency `tf` gets on a document whose norm byte is `norm`.
    fn bm25(scorer: &TermScorer, tf: u32, norm: u8) -> f64 {
        scorer.score(&[Some(FieldHit { tf, norm })])
    }

    struct Indexed {
        schema: Arc<CollectionSchema>,
        segment: Arc<Segment>,
        body: FieldId,
        stats: SegmentStatistics,
    }

    impl Indexed {
        fn view(&self) -> SegmentView<'_> {
            SegmentView::new(&self.segment, None)
        }
    }

    /// Documents analyzed with [`Synonyms`]: doc 0 is one `ant bee`, doc 1 is two, doc 2 has the words reversed.
    fn indexed() -> Indexed {
        let schema = SchemaBuilder::new("c")
            .field(
                "body",
                FieldKind::Text(TextOptions {
                    analyzer: AnalyzerRef::builtin("whitespace").unwrap(),
                    positions: true,
                    ..TextOptions::default()
                }),
            )
            .build()
            .unwrap();
        let body = schema.field("body").unwrap().id();
        let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        let stacking = Analyzer::builder(WhitespaceTokenizer)
            .filter(Synonyms)
            .build();
        assert!(stacking.stacks_tokens());
        // A schema can only name a built-in analyzer, and none of them stacks tokens.
        let index = usize::try_from(body.get()).unwrap();
        let FieldBuilder::Text { analyzer, .. } = &mut builder.fields[index] else {
            panic!("body is a text field");
        };
        *analyzer = Arc::new(stacking);
        for (id, text) in [
            ("once", "ant bee"),
            ("twice", "ant bee x ant bee"),
            ("order", "bee ant"),
        ] {
            builder
                .add(&Document::new(id).with(body, Value::String(text.into())))
                .unwrap();
        }
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&builder, meta).finish().unwrap();
        let schema = Arc::new(schema);
        let segment = Arc::new(Segment::open(ByteSource::new(bytes.into()), &schema).unwrap());
        let stats = SegmentStatistics::new(&schema, &[Arc::clone(&segment)]);
        Indexed {
            schema,
            segment,
            body,
            stats,
        }
    }

    /// `got` is the phrase score. It must be the score of `freq` distinct starts, which is below the score of
    /// counting every stacked copy (`copies`).
    fn assert_starts(
        scorer: &TermScorer,
        norms: &mut impl NormReader,
        doc: u32,
        freq: u32,
        copies: u32,
        got: f64,
    ) {
        let norm = norms.get(doc).unwrap();
        let want = bm25(scorer, freq, norm);
        assert!(
            want < bm25(scorer, copies, norm),
            "doc {doc}: the test cannot tell {freq} starts from {copies}"
        );
        assert_eq!(got, want, "doc {doc}");
    }

    /// A document that stacks tokens at one position counts each phrase start once, not once per copy
    /// (SPEC §7.2). Removing the dedup in `phrase_freq` makes the stacked document score as if every copy
    /// were its own start.
    #[test]
    fn stacked_document_tokens_count_a_phrase_start_once() {
        let indexed = indexed();
        let view = indexed.view();
        let body = indexed.body;

        // The term's own postings count every stacked copy. `ant` is at 0 twice, then at 0 and 3 twice.
        {
            let mut ant = view.postings(body, b"ant").unwrap().unwrap();
            assert_eq!(ant.next().unwrap(), Some(0));
            assert_eq!(
                (ant.freq().unwrap(), ant.positions().unwrap()),
                (2, &[0, 0][..])
            );
            assert_eq!(ant.next().unwrap(), Some(1));
            assert_eq!(
                (ant.freq().unwrap(), ant.positions().unwrap()),
                (4, &[0, 0, 3, 3][..])
            );
            assert_eq!(ant.next().unwrap(), Some(2));
            assert_eq!(ant.positions().unwrap(), &[1, 1]);
            let mut bee = view.postings(body, b"bee").unwrap().unwrap();
            assert_eq!(bee.next().unwrap(), Some(0));
            assert_eq!(bee.positions().unwrap(), &[1, 1]);
        }

        let ant_bee = phrase(body, &[("ant", 0), ("bee", 1)]);
        let got = scores(
            &SearchPlan::new(
                &Query::Phrase(ant_bee.clone()),
                &indexed.schema,
                &indexed.stats,
            )
            .unwrap(),
            &view,
        );
        assert_eq!(got.iter().map(|hit| hit.0).collect::<Vec<_>>(), [0, 1]);
        // `insect` sits on `bee`, so the synonym phrase matches the same documents.
        let synonym = scores(
            &SearchPlan::new(
                &Query::Phrase(phrase(body, &[("ant", 0), ("insect", 1)])),
                &indexed.schema,
                &indexed.stats,
            )
            .unwrap(),
            &view,
        );
        assert_eq!(synonym.iter().map(|hit| hit.0).collect::<Vec<_>>(), [0, 1]);

        let scorer = TermScorer::for_phrase(&ant_bee, &indexed.schema, &indexed.stats).unwrap();
        let mut norms = view.norms(body).unwrap();
        // Distinct starts, then the count if every stacked copy were its own start.
        for (doc, freq, copies) in [(0, 1_u32, 2_u32), (1, 2, 4)] {
            assert_starts(
                &scorer,
                &mut norms,
                doc,
                freq,
                copies,
                got[usize::try_from(doc).unwrap()].1,
            );
        }

        // A one-term phrase equals a term, except a token stacked on itself counts once (SPEC §7.2).
        let one = phrase(body, &[("ant", 0)]);
        let term = TermQuery {
            field: body,
            term: "ant".into(),
        };
        let phrase_hits = scores(
            &SearchPlan::new(&Query::Phrase(one.clone()), &indexed.schema, &indexed.stats).unwrap(),
            &view,
        );
        let term_hits = scores(
            &SearchPlan::new(&Query::Term(term.clone()), &indexed.schema, &indexed.stats).unwrap(),
            &view,
        );
        assert_eq!(
            phrase_hits.iter().map(|hit| hit.0).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        let phrase_scorer = TermScorer::for_phrase(&one, &indexed.schema, &indexed.stats).unwrap();
        let term_scorer = TermScorer::for_term(&term, &indexed.schema, &indexed.stats).unwrap();
        for (doc, freq, copies) in [(0, 1_u32, 2_u32), (1, 2, 4), (2, 1, 2)] {
            let i = usize::try_from(doc).unwrap();
            let norm = norms.get(doc).unwrap();
            let phrase_score = phrase_hits[i].1;
            assert_eq!(phrase_score, bm25(&phrase_scorer, freq, norm), "doc {doc}");
            assert_eq!(
                term_hits[i].1,
                bm25(&term_scorer, copies, norm),
                "doc {doc}"
            );
            assert!(
                phrase_score < term_hits[i].1,
                "doc {doc}: a phrase counts the stacked position once"
            );
        }
    }
}

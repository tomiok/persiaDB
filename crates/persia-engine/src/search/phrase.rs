//! Phrase matching with positions (ROADMAP 7.4.2): the documents holding every term of a phrase, leapfrogged as a
//! conjunction, where the terms also stand at their relative positions; scored from the phrase frequency (SPEC §7.2).

use crate::score::{FieldHit, TermScorer};
use crate::segment::ReadError;
use crate::view::{NormReader, PostingsIterator, SearchView};

use super::boolean::Conjunction;
use super::{Matcher, Pos};

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
}

/// The documents holding a phrase, at least one term.
pub(super) struct PhraseMatcher<'a, V: SearchView + 'a> {
    terms: Conjunction<PhrasePostings<V::Postings<'a>>>,
    norms: V::Norms<'a>,
    scorer: &'a TermScorer,
    /// The phrase frequency of the current document, at least 1.
    freq: u32,
    pos: Pos,
}

impl<'a, V: SearchView + 'a> PhraseMatcher<'a, V> {
    pub(super) fn new(
        terms: Vec<PhrasePostings<V::Postings<'a>>>,
        norms: V::Norms<'a>,
        scorer: &'a TermScorer,
    ) -> Self {
        Self {
            terms: Conjunction::new(terms),
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
        let hit = FieldHit {
            tf: self.freq,
            norm: self.norms.get(doc)?,
        };
        Ok(self.scorer.score(&[Some(hit)]))
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

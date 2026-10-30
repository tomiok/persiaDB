//! The matcher of one query word (ROADMAP 7.4.1): a `term` (one field) or a `combined` (several), the documents
//! holding the word in any of its fields, scored as one BM25F term (SPEC §7.2).

use crate::score::{FieldHit, TermScorer};
use crate::segment::ReadError;
use crate::view::{NormReader, PostingsIterator, SearchView};

use super::{Matcher, Pos};

/// One field of the word that the view holds the term in.
pub(super) struct WordField<'a, V: SearchView + 'a> {
    /// The field's index in the scorer's fields.
    pub(super) slot: usize,
    pub(super) postings: V::Postings<'a>,
    /// `None` for a `keyword` field, which has no norms.
    pub(super) norms: Option<V::Norms<'a>>,
}

/// The union of a word's fields; at least one.
pub(super) struct WordMatcher<'a, V: SearchView> {
    fields: Vec<WordField<'a, V>>,
    scorer: &'a TermScorer,
    hits: Vec<Option<FieldHit>>,
    pos: Pos,
}

impl<'a, V: SearchView + 'a> WordMatcher<'a, V> {
    pub(super) fn new(fields: Vec<WordField<'a, V>>, scorer: &'a TermScorer) -> Self {
        Self {
            fields,
            hits: vec![None; scorer.fields().len()],
            scorer,
            pos: Pos::Unstarted,
        }
    }
}

impl<'a, V: SearchView + 'a> Matcher for WordMatcher<'a, V> {
    fn cost(&self) -> u64 {
        self.fields.iter().fold(0_u64, |sum, f| {
            sum.saturating_add(u64::from(f.postings.df()))
        })
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
        let started = match self.pos {
            Pos::At(d) if d >= target => return Ok(Some(d)),
            Pos::Done => return Ok(None),
            Pos::Unstarted => false,
            Pos::At(_) => true,
        };
        self.pos = Pos::Done;
        let mut min = None::<u32>;
        for field in &mut self.fields {
            let doc = match field.postings.doc() {
                None if started => None,
                Some(d) if d >= target => Some(d),
                _ => field.postings.advance(target)?,
            };
            if let Some(d) = doc {
                min = Some(min.map_or(d, |m| m.min(d)));
            }
        }
        self.pos = Pos::of(min);
        Ok(min)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        let Some(doc) = self.pos.doc() else {
            return Ok(0.0);
        };
        self.hits.fill(None);
        for field in &mut self.fields {
            if field.postings.doc() != Some(doc) {
                continue;
            }
            let hit = FieldHit {
                tf: field.postings.freq()?,
                norm: match field.norms.as_mut() {
                    Some(norms) => norms.get(doc)?,
                    None => 0,
                },
            };
            if let Some(slot) = self.hits.get_mut(field.slot) {
                *slot = Some(hit);
            }
        }
        Ok(self.scorer.score(&self.hits))
    }
}

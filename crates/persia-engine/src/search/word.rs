//! The matcher of one query word (ROADMAP 7.4.1): a `term` (one field) or a `combined` (several), the documents
//! holding the word in any of its fields, scored as one BM25F term (SPEC §7.2).
//!
//! Its bounds put the fields' `max_tf` and `min_norm` in place of a document's frequency and norm (ROADMAP 7.3.3):
//! the term's for the whole list, a postings block's for [`Matcher::block_bound`]. Given a threshold, it skips what
//! cannot beat it: under [`Pruning::BlockMaxWand`] the blocks whose bound is at most the threshold, under
//! [`Pruning::MaxScore`] the whole list when its bound is.

use crate::schema::FieldId;
use crate::score::{FieldHit, TermScorer};
use crate::segment::ReadError;
use crate::view::{PostingsIterator, SearchView};

use super::{Matcher, Pos, Pruning, ScoreBound, bounded_norm, raise};

/// One field of the word that the view holds the term in.
pub(super) struct WordField<'a, V: SearchView + 'a> {
    /// The field's index in the scorer's fields.
    pub(super) slot: usize,
    pub(super) postings: V::Postings<'a>,
    /// The field, for errors.
    pub(super) field: FieldId,
    /// `None` for a `keyword` field, which has no norms.
    pub(super) norms: Option<V::Norms<'a>>,
}

/// The union of a word's fields; at least one.
pub(super) struct WordMatcher<'a, V: SearchView> {
    fields: Vec<WordField<'a, V>>,
    scorer: &'a TermScorer,
    hits: Vec<Option<FieldHit>>,
    pos: Pos,
    /// [`Matcher::max_score`].
    max: f64,
    pruning: Pruning,
    /// The threshold of [`Matcher::set_min_competitive`], when pruning.
    min: Option<f64>,
    /// The last bound checked against `min`: it covers every document from where it was taken through `up_to`.
    region: Option<ScoreBound>,
}

impl<'a, V: SearchView + 'a> WordMatcher<'a, V> {
    pub(super) fn new(
        fields: Vec<WordField<'a, V>>,
        scorer: &'a TermScorer,
        pruning: Pruning,
    ) -> Self {
        let mut hits = vec![None; scorer.fields().len()];
        for field in &fields {
            if let Some(slot) = hits.get_mut(field.slot) {
                *slot = Some(field.postings.term_bound());
            }
        }
        let max = scorer.score(&hits);
        Self {
            fields,
            hits,
            scorer,
            pos: Pos::Unstarted,
            max,
            pruning,
            min: None,
            region: None,
        }
    }

    /// [`Matcher::block_bound`], on any error leaving some field's postings exhausted.
    fn field_blocks(&mut self, target: u32) -> Result<Option<ScoreBound>, ReadError> {
        self.hits.fill(None);
        let mut up_to = None::<u32>;
        for field in &mut self.fields {
            let Some(block) = field.postings.shallow_advance(target)? else {
                continue;
            };
            up_to = Some(up_to.map_or(block.last_docid, |u| u.min(block.last_docid)));
            if let Some(slot) = self.hits.get_mut(field.slot) {
                *slot = Some(FieldHit::from(block));
            }
        }
        Ok(up_to.map(|up_to| ScoreBound {
            up_to,
            score: self.scorer.score(&self.hits),
        }))
    }

    /// The current document's hit in each field of the scorer (`None` where it lacks the term), what
    /// [`Matcher::score`] scores; every field `None` when not on a document. A norm below its block's `min_norm` is
    /// corruption.
    pub(super) fn field_hits(&mut self) -> Result<&[Option<FieldHit>], ReadError> {
        self.hits.fill(None);
        let Some(doc) = self.pos.doc() else {
            return Ok(&self.hits);
        };
        for field in &mut self.fields {
            if field.postings.doc() != Some(doc) {
                continue;
            }
            let hit = FieldHit {
                tf: field.postings.freq()?,
                norm: match field.norms.as_mut() {
                    Some(norms) => {
                        let min_norm = field.postings.block_max().map(|b| b.min_norm);
                        bounded_norm(norms, field.field, doc, min_norm)?
                    }
                    None => 0,
                },
            };
            if let Some(slot) = self.hits.get_mut(field.slot) {
                *slot = Some(hit);
            }
        }
        Ok(&self.hits)
    }

    /// The first document at or after `target` of any field, stepping each field's postings only when behind.
    #[inline]
    fn step(&mut self, target: u32, started: bool) -> Result<Option<u32>, ReadError> {
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
        Ok(min)
    }

    /// From `target` on, the first range whose bound beats `min`, as its first and last docid; `None` when no
    /// later document can. The documents it steps over cannot score above `min`.
    fn competitive(&mut self, mut target: u32, min: f64) -> Result<Option<(u32, u32)>, ReadError> {
        loop {
            let region = match self.region {
                // Taken at or before `target` (targets only grow), so it covers `target` too.
                Some(r) if target <= r.up_to => r,
                _ => {
                    let bound = match self.pruning {
                        Pruning::BlockMaxWand => self.block_bound(target)?,
                        Pruning::MaxScore | Pruning::Exhaustive => Some(ScoreBound {
                            up_to: u32::MAX,
                            score: self.max,
                        }),
                    };
                    let Some(bound) = bound else {
                        return Ok(None);
                    };
                    self.region = Some(bound);
                    bound
                }
            };
            if region.score > min {
                return Ok(Some((target, region.up_to)));
            }
            // `up_to` is at least `target` (a block from `target` on ends there or later); `max` keeps the walk moving
            // whatever a bound says.
            let Some(after) = region.up_to.max(target).checked_add(1) else {
                return Ok(None);
            };
            target = after;
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

    fn advance(&mut self, mut target: u32) -> Result<Option<u32>, ReadError> {
        let mut started = match self.pos {
            Pos::At(d) if d >= target => return Ok(Some(d)),
            Pos::Done => return Ok(None),
            Pos::Unstarted => false,
            Pos::At(_) => true,
        };
        self.pos = Pos::Done;
        let Some(min) = self.min else {
            // No threshold: the plain union of the fields, as in a conjunction's leapfrog.
            let doc = self.step(target, started)?;
            self.pos = Pos::of(doc);
            return Ok(doc);
        };
        loop {
            let Some((from, up_to)) = self.competitive(target, min)? else {
                return Ok(None);
            };
            target = from;
            let Some(doc) = self.step(target, started)? else {
                return Ok(None);
            };
            started = true;
            // Past the region whose bound beat `min`: check the next one's before scoring there.
            if doc <= up_to {
                self.pos = Pos::At(doc);
                return Ok(Some(doc));
            }
            target = doc;
        }
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        if self.pos.doc().is_none() {
            return Ok(0.0);
        }
        let scorer = self.scorer;
        Ok(scorer.score(self.field_hits()?))
    }

    fn max_score(&self) -> f64 {
        self.max
    }

    /// The fields' blocks holding their first document at or after `target`: through the first of those blocks to
    /// end, each field's documents in that range are in its block, so its `max_tf` and `min_norm` bound them.
    fn block_bound(&mut self, target: u32) -> Result<Option<ScoreBound>, ReadError> {
        let bound = self.field_blocks(target);
        if bound.is_err() {
            // A field whose postings failed is exhausted: so is the word, rather than scoring without it.
            self.pos = Pos::Done;
        }
        bound
    }

    fn set_min_competitive(&mut self, score: f64) {
        if self.pruning != Pruning::Exhaustive {
            raise(&mut self.min, score);
        }
    }
}

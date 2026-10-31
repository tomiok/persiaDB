//! Block-max WAND (ROADMAP 7.4.4.2): a disjunction that, given a threshold, finds the pivot (the first document
//! whose clauses' whole-list bounds can beat it), checks the bounds of the postings blocks there (the Skip entries,
//! SPEC §5.3.2), and scores a document only when they can too; when they cannot, it jumps past the first of those
//! blocks to end.
//!
//! Every bound is summed in query order, as the score is (SPEC §7.2), so a bound is at least the score exactly
//! ([`sum_in_order`]): a document is skipped only when it provably scores at most the threshold.

use crate::segment::ReadError;

use super::{Matcher, Pos, ScoreBound, raise, sum_in_order};

/// A disjunction of at least two clauses, scored as [`super::boolean::Disjunction`].
pub(super) struct BlockMaxWand<M> {
    /// In query order.
    clauses: Vec<M>,
    /// Each clause's [`Matcher::max_score`].
    bounds: Vec<f64>,
    min: Option<f64>,
    /// Scratch: clause indices on a document, by document.
    order: Vec<(u32, usize)>,
    /// Scratch: per clause, what it adds to a bound.
    parts: Vec<f64>,
    /// Per clause, the last [`Matcher::block_bound`] and the target it was taken at: it still holds for a later
    /// target up to its `up_to` (or for any later target, when `None`).
    blocks: Vec<Option<(u32, Option<ScoreBound>)>>,
    pos: Pos,
    score: f64,
}

impl<M: Matcher> BlockMaxWand<M> {
    pub(super) fn new(clauses: Vec<M>) -> Self {
        Self {
            bounds: clauses.iter().map(Matcher::max_score).collect(),
            order: Vec::with_capacity(clauses.len()),
            parts: vec![0.0; clauses.len()],
            blocks: vec![None; clauses.len()],
            clauses,
            min: None,
            pos: Pos::Unstarted,
            score: 0.0,
        }
    }

    /// Moves every clause before `target` to its first document at or after it.
    fn catch_up(&mut self, target: u32) -> Result<(), ReadError> {
        for clause in &mut self.clauses {
            if clause.doc().is_none_or(|d| d < target) {
                clause.advance(target)?;
            }
        }
        Ok(())
    }

    /// The clauses still on a document, sorted by it (then query order).
    fn sort(&mut self) {
        self.order.clear();
        self.order.extend(
            self.clauses
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.doc().map(|d| (d, i))),
        );
        self.order.sort_unstable();
    }

    /// The pivot: the smallest `p` such that the clauses `order[..=p]` can beat `min` together. Every document
    /// before `order[p]`'s matches only clauses of `order[..p]`, whose bounds cannot.
    fn pivot(&mut self, min: f64) -> Option<usize> {
        self.parts.fill(0.0);
        for (p, &(_, i)) in self.order.iter().enumerate() {
            if let (Some(part), Some(&bound)) = (self.parts.get_mut(i), self.bounds.get(i)) {
                *part = bound;
            }
            if sum_in_order(self.parts.iter().copied()) > min {
                return Some(p);
            }
        }
        None
    }

    /// Clause `i`'s block bound at `target`, from the cache while it covers `target`.
    fn block_bound(&mut self, i: usize, target: u32) -> Result<Option<ScoreBound>, ReadError> {
        if let Some(&Some((from, bound))) = self.blocks.get(i)
            && from <= target
            && bound.is_none_or(|b| target <= b.up_to)
        {
            return Ok(bound);
        }
        let bound = match self.clauses.get_mut(i) {
            Some(c) => c.block_bound(target)?,
            None => None,
        };
        if let Some(slot) = self.blocks.get_mut(i) {
            *slot = Some((target, bound));
        }
        Ok(bound)
    }

    /// The exact score of `doc`, which every clause is at or after: the matching clauses' scores in query order.
    fn score_of(&mut self, doc: u32) -> Result<f64, ReadError> {
        let mut sum = 0.0;
        for clause in &mut self.clauses {
            if clause.doc() == Some(doc) {
                sum += clause.score()?;
            }
        }
        Ok(sum)
    }

    /// From `target` (every clause at or after it), the next document to score, or `None`.
    fn find(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        self.catch_up(target)?;
        loop {
            self.sort();
            let Some(&(first, _)) = self.order.first() else {
                return Ok(None);
            };
            let Some(min) = self.min else {
                return Ok(Some(first));
            };
            let Some(p) = self.pivot(min) else {
                // Every clause together cannot beat `min`: no later document can.
                return Ok(None);
            };
            let Some(&(pivot, _)) = self.order.get(p) else {
                return Ok(None);
            };
            // The clauses that can match a document from `pivot` until the next clause's document.
            let lead = self.order.partition_point(|&(d, _)| d <= pivot);
            let next = self.order.get(lead).map(|&(d, _)| d);
            self.parts.fill(0.0);
            let mut up_to = u32::MAX;
            for n in 0..lead {
                let Some(&(_, i)) = self.order.get(n) else {
                    continue;
                };
                let block = self.block_bound(i, pivot)?;
                // A clause without a block from `pivot` on matches nothing there: it adds nothing.
                if let (Some(b), Some(part)) = (block, self.parts.get_mut(i)) {
                    *part = b.score;
                    up_to = up_to.min(b.up_to);
                }
            }
            if sum_in_order(self.parts.iter().copied()) <= min {
                // From `pivot` through the first block's end, and before `next`, only those clauses match, and
                // within their blocks: no document there can beat `min`.
                let skip_to = match (up_to.checked_add(1), next) {
                    (Some(end), Some(n)) => end.min(n),
                    (Some(end), None) => end,
                    (None, Some(n)) => n,
                    (None, None) => return Ok(None),
                };
                // Past `pivot` whatever the bounds say (`up_to` is at least `pivot`), so the walk always moves on.
                let Some(skip_to) = pivot.checked_add(1).map(|p| skip_to.max(p)) else {
                    return Ok(None);
                };
                self.catch_up(skip_to)?;
                continue;
            }
            if first == pivot {
                return Ok(Some(pivot));
            }
            // The documents before `pivot` cannot beat `min`.
            self.catch_up(pivot)?;
        }
    }
}

impl<M: Matcher> Matcher for BlockMaxWand<M> {
    fn cost(&self) -> u64 {
        self.clauses
            .iter()
            .fold(0_u64, |sum, m| sum.saturating_add(m.cost()))
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
        let Some(doc) = self.find(target)? else {
            return Ok(None);
        };
        self.score = self.score_of(doc)?;
        self.pos = Pos::At(doc);
        Ok(Some(doc))
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        Ok(if self.pos.doc().is_some() {
            self.score
        } else {
            0.0
        })
    }

    fn max_score(&self) -> f64 {
        sum_in_order(self.bounds.iter().copied())
    }

    fn set_min_competitive(&mut self, score: f64) {
        raise(&mut self.min, score);
    }
}

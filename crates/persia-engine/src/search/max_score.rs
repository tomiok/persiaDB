//! `MaxScore` (ROADMAP 7.4.4.1): a disjunction that, given a threshold, leaves out the clauses whose bounds
//! together cannot beat it (non-essential), walks the documents of the others (essential), and looks the
//! non-essential ones up on those documents only while the document's bound still beats the threshold.
//!
//! Every bound is summed in query order, as the score is (SPEC §7.2), so a bound is at least the score exactly
//! ([`sum_in_order`]): a document is left out only when it provably scores at most the threshold.

use crate::segment::ReadError;

use super::{Matcher, Pos, raise, sum_in_order};

/// What a clause adds to the current document's bound.
#[derive(Debug, Clone, Copy)]
enum Part {
    /// Not looked up yet: its [`Matcher::max_score`].
    Unknown,
    /// On the document, with this score.
    Hit(f64),
    /// Not on the document.
    Miss,
}

/// A disjunction of at least two clauses, scored as [`super::boolean::Disjunction`].
pub(super) struct MaxScore<M> {
    /// In query order.
    clauses: Vec<M>,
    /// Each clause's [`Matcher::max_score`].
    bounds: Vec<f64>,
    /// Clause indices by bound, smallest first; `by_bound[..essential]` are non-essential.
    by_bound: Vec<usize>,
    essential: usize,
    min: Option<f64>,
    parts: Vec<Part>,
    pos: Pos,
    score: f64,
}

impl<M: Matcher> MaxScore<M> {
    pub(super) fn new(clauses: Vec<M>) -> Self {
        let bounds: Vec<f64> = clauses.iter().map(Matcher::max_score).collect();
        let mut by_bound: Vec<usize> = (0..clauses.len()).collect();
        // Stable: equal bounds keep query order, so the walk is deterministic.
        by_bound.sort_by(|&a, &b| {
            let bound = |i: usize| bounds.get(i).copied().unwrap_or(f64::INFINITY);
            bound(a).total_cmp(&bound(b))
        });
        Self {
            parts: vec![Part::Miss; clauses.len()],
            clauses,
            bounds,
            by_bound,
            essential: 0,
            min: None,
            pos: Pos::Unstarted,
            score: 0.0,
        }
    }

    /// The bound of the clauses `include` names, summed in query order.
    fn bound_of(&self, include: impl Fn(usize) -> bool) -> f64 {
        sum_in_order(
            self.bounds
                .iter()
                .enumerate()
                .map(|(i, &b)| if include(i) { b } else { 0.0 }),
        )
    }

    /// The current document's bound: the scores found, and the bounds of the clauses not looked up.
    fn part_bound(&self) -> f64 {
        sum_in_order(self.parts.iter().zip(&self.bounds).map(|(p, &b)| match *p {
            Part::Unknown => b,
            Part::Hit(s) => s,
            Part::Miss => 0.0,
        }))
    }

    /// The first essential document at or after `target`, every essential clause moved to at least it.
    fn candidate(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        let mut min = None::<u32>;
        for &i in self.by_bound.get(self.essential..).unwrap_or_default() {
            let Some(clause) = self.clauses.get_mut(i) else {
                continue;
            };
            let doc = match clause.doc() {
                Some(d) if d >= target => Some(d),
                _ => clause.advance(target)?,
            };
            if let Some(d) = doc {
                min = Some(min.map_or(d, |m| m.min(d)));
            }
        }
        Ok(min)
    }

    /// Scores `doc` if its bound beats the threshold all along; `None` once it cannot.
    fn evaluate(&mut self, doc: u32) -> Result<Option<f64>, ReadError> {
        for (n, &i) in self.by_bound.iter().enumerate() {
            let essential = n >= self.essential;
            let part = match self.clauses.get_mut(i) {
                Some(c) if essential && c.doc() == Some(doc) => Part::Hit(c.score()?),
                Some(_) if !essential => Part::Unknown,
                _ => Part::Miss,
            };
            if let Some(slot) = self.parts.get_mut(i) {
                *slot = part;
            }
        }
        // The non-essential clauses, largest bound first: each lookup can only lower the bound.
        for n in (0..self.essential).rev() {
            if let Some(min) = self.min
                && self.part_bound() <= min
            {
                return Ok(None);
            }
            let Some(&i) = self.by_bound.get(n) else {
                continue;
            };
            let mut part = Part::Miss;
            if let Some(c) = self.clauses.get_mut(i)
                && c.advance(doc)? == Some(doc)
            {
                part = Part::Hit(c.score()?);
            }
            if let Some(slot) = self.parts.get_mut(i) {
                *slot = part;
            }
        }
        // As the plain disjunction sums: the matching clauses' scores in query order.
        let mut sum = 0.0;
        for part in &self.parts {
            if let Part::Hit(s) = *part {
                sum += s;
            }
        }
        Ok(Some(sum))
    }
}

impl<M: Matcher> Matcher for MaxScore<M> {
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

    fn advance(&mut self, mut target: u32) -> Result<Option<u32>, ReadError> {
        match self.pos {
            Pos::At(d) if d >= target => return Ok(Some(d)),
            Pos::Done => return Ok(None),
            Pos::Unstarted | Pos::At(_) => {}
        }
        self.pos = Pos::Done;
        loop {
            let Some(doc) = self.candidate(target)? else {
                return Ok(None);
            };
            if let Some(score) = self.evaluate(doc)? {
                self.score = score;
                self.pos = Pos::At(doc);
                return Ok(Some(doc));
            }
            let Some(after) = doc.checked_add(1) else {
                return Ok(None);
            };
            target = after;
        }
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

    /// Moves clauses into the non-essential set while their bounds together, in query order, stay at most `score`:
    /// a document only they match cannot beat it.
    fn set_min_competitive(&mut self, score: f64) {
        if !raise(&mut self.min, score) {
            return;
        }
        let mut in_set = vec![false; self.clauses.len()];
        for &i in self.by_bound.get(..self.essential).unwrap_or_default() {
            if let Some(slot) = in_set.get_mut(i) {
                *slot = true;
            }
        }
        while let Some(&i) = self.by_bound.get(self.essential) {
            if let Some(slot) = in_set.get_mut(i) {
                *slot = true;
            }
            if self.bound_of(|j| in_set.get(j).copied().unwrap_or(false)) > score {
                break;
            }
            self.essential += 1;
        }
    }
}

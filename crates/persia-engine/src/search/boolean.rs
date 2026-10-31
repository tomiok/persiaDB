//! Boolean matchers (ROADMAP 7.4.1): AND, OR, NOT, a required clause with optional ones, every document, none,
//! and the score modifiers of `boost` and `constant_score`.

use crate::segment::ReadError;
use crate::view::SearchView;

use super::{Matcher, Pos, ScoreBound, sum_in_order};

/// The documents every clause matches (AND), leapfrogging from the cheapest clause; the score is the sum of the
/// clauses', added in query order (SPEC §7.2).
pub(super) struct Conjunction<M> {
    /// By cost, cheapest first.
    clauses: Vec<M>,
    /// `clauses` indices in query order, for the score.
    order: Vec<usize>,
    pos: Pos,
}

impl<M: Matcher> Conjunction<M> {
    /// At least one clause; with none, the conjunction matches nothing.
    pub(super) fn new(clauses: Vec<M>) -> Self {
        let mut indexed: Vec<(usize, M)> = clauses.into_iter().enumerate().collect();
        // Stable: equal costs keep query order, so the walk is deterministic.
        indexed.sort_by_key(|(_, m)| m.cost());
        let mut order = vec![0; indexed.len()];
        for (sorted, &(original, _)) in indexed.iter().enumerate() {
            if let Some(slot) = order.get_mut(original) {
                *slot = sorted;
            }
        }
        Self {
            clauses: indexed.into_iter().map(|(_, m)| m).collect(),
            order,
            pos: Pos::Unstarted,
        }
    }

    /// The clauses, cheapest first.
    pub(super) fn clauses_mut(&mut self) -> &mut [M] {
        &mut self.clauses
    }

    /// The first document at or after `target` every clause holds.
    fn align(&mut self, mut target: u32) -> Result<Option<u32>, ReadError> {
        let Some((lead, rest)) = self.clauses.split_first_mut() else {
            return Ok(None);
        };
        'candidates: loop {
            let Some(doc) = lead.advance(target)? else {
                return Ok(None);
            };
            for clause in rest.iter_mut() {
                match clause.advance(doc)? {
                    None => return Ok(None),
                    Some(d) if d > doc => {
                        target = d;
                        continue 'candidates;
                    }
                    Some(_) => {}
                }
            }
            return Ok(Some(doc));
        }
    }
}

impl<M: Matcher> Matcher for Conjunction<M> {
    fn cost(&self) -> u64 {
        self.clauses.first().map_or(0, Matcher::cost)
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
        let doc = self.align(target)?;
        self.pos = Pos::of(doc);
        Ok(doc)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        if self.pos.doc().is_none() {
            return Ok(0.0);
        }
        let mut sum = 0.0;
        for &i in &self.order {
            if let Some(clause) = self.clauses.get_mut(i) {
                sum += clause.score()?;
            }
        }
        Ok(sum)
    }

    fn max_score(&self) -> f64 {
        sum_in_order(
            self.order
                .iter()
                .filter_map(|&i| self.clauses.get(i).map(Matcher::max_score)),
        )
    }
}

/// The documents any clause matches (OR); the score is the sum of the matching clauses', in query order.
pub(super) struct Disjunction<M> {
    clauses: Vec<M>,
    pos: Pos,
}

impl<M: Matcher> Disjunction<M> {
    pub(super) fn new(clauses: Vec<M>) -> Self {
        Self {
            clauses,
            pos: Pos::Unstarted,
        }
    }
}

impl<M: Matcher> Matcher for Disjunction<M> {
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
        let started = match self.pos {
            Pos::At(d) if d >= target => return Ok(Some(d)),
            Pos::Done => return Ok(None),
            Pos::Unstarted => false,
            Pos::At(_) => true,
        };
        self.pos = Pos::Done;
        let mut min = None::<u32>;
        for clause in &mut self.clauses {
            // Once started, a clause without a document is exhausted; one at or after `target` stays.
            let doc = match clause.doc() {
                None if started => None,
                Some(d) if d >= target => Some(d),
                _ => clause.advance(target)?,
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
        let mut sum = 0.0;
        for clause in &mut self.clauses {
            if clause.doc() == Some(doc) {
                sum += clause.score()?;
            }
        }
        Ok(sum)
    }

    fn max_score(&self) -> f64 {
        sum_in_order(self.clauses.iter().map(Matcher::max_score))
    }
}

/// The documents `required` matches and `excluded` does not (NOT); scored by `required` alone.
pub(super) struct Exclusion<R, E> {
    required: R,
    excluded: E,
    pos: Pos,
}

impl<R: Matcher, E: Matcher> Exclusion<R, E> {
    pub(super) fn new(required: R, excluded: E) -> Self {
        Self {
            required,
            excluded,
            pos: Pos::Unstarted,
        }
    }
}

impl<R: Matcher, E: Matcher> Matcher for Exclusion<R, E> {
    fn cost(&self) -> u64 {
        self.required.cost()
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
            let Some(doc) = self.required.advance(target)? else {
                return Ok(None);
            };
            if self.excluded.advance(doc)? != Some(doc) {
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
        if self.pos.doc().is_none() {
            return Ok(0.0);
        }
        self.required.score()
    }

    fn max_score(&self) -> f64 {
        self.required.max_score()
    }

    fn block_bound(&mut self, target: u32) -> Result<Option<ScoreBound>, ReadError> {
        self.required.block_bound(target)
    }

    /// The score is `required`'s, so what it may skip, this may too.
    fn set_min_competitive(&mut self, score: f64) {
        self.required.set_min_competitive(score);
    }
}

/// The documents `required` matches, scored by it plus `optional` where that matches too (a `bool`'s `should`
/// clauses beside `must` ones).
pub(super) struct Optional<R, O> {
    required: R,
    optional: O,
}

impl<R: Matcher, O: Matcher> Optional<R, O> {
    pub(super) fn new(required: R, optional: O) -> Self {
        Self { required, optional }
    }
}

impl<R: Matcher, O: Matcher> Matcher for Optional<R, O> {
    fn cost(&self) -> u64 {
        self.required.cost()
    }

    fn doc(&self) -> Option<u32> {
        self.required.doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        self.required.next()
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        self.required.advance(target)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        let Some(doc) = self.required.doc() else {
            return Ok(0.0);
        };
        let mut score = self.required.score()?;
        // `required` only moves forward, so `optional` is only ever advanced forward too.
        if self.optional.advance(doc)? == Some(doc) {
            score += self.optional.score()?;
        }
        Ok(score)
    }

    fn max_score(&self) -> f64 {
        self.required.max_score() + self.optional.max_score()
    }
}

/// Every live document of a view, each scoring 0 (`match_all`, or a `bool` of `must_not` clauses alone).
pub(super) struct AllDocs<'v, V> {
    view: &'v V,
    pos: Pos,
}

impl<'v, V: SearchView> AllDocs<'v, V> {
    pub(super) fn new(view: &'v V) -> Self {
        Self {
            view,
            pos: Pos::Unstarted,
        }
    }
}

impl<V: SearchView> Matcher for AllDocs<'_, V> {
    fn cost(&self) -> u64 {
        u64::from(self.view.doc_count())
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
        let doc = (target..self.view.doc_count()).find(|&d| self.view.is_live(d));
        self.pos = Pos::of(doc);
        Ok(doc)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        Ok(0.0)
    }

    fn max_score(&self) -> f64 {
        0.0
    }
}

/// No document: a term the view does not hold, or a conjunction with such a clause.
pub(super) struct Empty;

impl Matcher for Empty {
    fn cost(&self) -> u64 {
        0
    }

    fn doc(&self) -> Option<u32> {
        None
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        Ok(None)
    }

    fn advance(&mut self, _: u32) -> Result<Option<u32>, ReadError> {
        Ok(None)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        Ok(0.0)
    }

    fn max_score(&self) -> f64 {
        0.0
    }
}

/// How a [`Scaled`] matcher changes its inner matcher's score.
#[derive(Debug, Clone, Copy)]
pub(super) enum Scale {
    /// `boost`: multiplies it.
    Boost(f64),
    /// `constant_score`: replaces it.
    Constant(f64),
}

/// A matcher whose score a [`Scale`] changes; it matches what the inner one matches.
pub(super) struct Scaled<M> {
    inner: M,
    scale: Scale,
}

impl<M: Matcher> Scaled<M> {
    pub(super) fn new(inner: M, scale: Scale) -> Self {
        Self { inner, scale }
    }
}

impl<M: Matcher> Matcher for Scaled<M> {
    fn cost(&self) -> u64 {
        self.inner.cost()
    }

    fn doc(&self) -> Option<u32> {
        self.inner.doc()
    }

    fn next(&mut self) -> Result<Option<u32>, ReadError> {
        self.inner.next()
    }

    fn advance(&mut self, target: u32) -> Result<Option<u32>, ReadError> {
        self.inner.advance(target)
    }

    fn score(&mut self) -> Result<f64, ReadError> {
        if self.inner.doc().is_none() {
            return Ok(0.0);
        }
        match self.scale {
            Scale::Boost(boost) => Ok(self.inner.score()? * boost),
            Scale::Constant(score) => Ok(score),
        }
    }

    fn max_score(&self) -> f64 {
        match self.scale {
            Scale::Boost(boost) => self.inner.max_score() * boost,
            Scale::Constant(score) => score,
        }
    }

    fn block_bound(&mut self, target: u32) -> Result<Option<ScoreBound>, ReadError> {
        Ok(match self.scale {
            // A positive factor keeps the order of scores, rounding included.
            Scale::Boost(boost) => self.inner.block_bound(target)?.map(|b| ScoreBound {
                up_to: b.up_to,
                score: b.score * boost,
            }),
            Scale::Constant(score) => Some(ScoreBound {
                up_to: u32::MAX,
                score,
            }),
        })
    }

    /// A boost passes on the largest inner score whose product is at most `score`: any inner score at most that
    /// one gives a product at most `score` too.
    fn set_min_competitive(&mut self, score: f64) {
        if let Scale::Boost(boost) = self.scale
            && let Some(inner) = unboosted(score, boost)
        {
            self.inner.set_min_competitive(inner);
        }
    }
}

/// The largest `x` found with `x · boost ≤ score` (as computed); `None` when the quotient is not finite.
fn unboosted(score: f64, boost: f64) -> Option<f64> {
    let mut x = score / boost;
    // `score / boost` is correctly rounded, so its product overshoots `score` by at most an ulp or two.
    for _ in 0..4 {
        if !x.is_finite() {
            return None;
        }
        if x * boost <= score {
            return Some(x);
        }
        x = x.next_down();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::unboosted;

    /// The threshold a boost passes on never lets through an inner score whose product beats the outer one.
    #[test]
    #[allow(clippy::cast_precision_loss)] // 53-bit values, exact
    fn unboosted_products_stay_at_most_the_threshold() {
        let mut x = 0x9E37_79B9_7F4A_7C15_u64;
        let mut unit = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1_u64 << 53) as f64
        };
        let mut found = 0;
        for _ in 0..100_000 {
            let score = unit() * 50.0;
            let boost = 0.01 + unit() * 20.0;
            if let Some(inner) = unboosted(score, boost) {
                found += 1;
                assert!(inner * boost <= score, "{inner} · {boost} > {score}");
            }
        }
        assert!(found > 99_000);
    }
}

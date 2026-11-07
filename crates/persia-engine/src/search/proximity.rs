//! Term proximity (ROADMAP 7.3.5, SPEC §7.2, ADR-0036): a `bool`'s bonus for its adjacent required words standing
//! close together in a field with positions.
//!
//! For each pair of adjacent `must` words, and each field with positions holding both terms, the least distance
//! `d` between an occurrence of the first (at `p`) and of the second (at `q`): `q − p` when `q > p`, else
//! `p − q + 1`. The pair's closeness is the best field's `1 / d²` up to [`WINDOW`], and the bonus
//! `Σ (λ · min(idf)) · closeness` in pair order. With every closeness 1 it is [`ProximityPlan::bound`]: products
//! are monotonic in a factor at most 1, and so is the sum, so the bonus never exceeds it (SPEC §7.3's pruning).

use crate::schema::FieldId;
use crate::segment::ReadError;
use crate::view::{PostingsIterator, SearchView};

use super::boolean::Bonus;

/// `λ`: an adjacent pair adds `min(idf)` of its two words.
pub(crate) const LAMBDA: f64 = 1.0;

/// The largest distance that counts: beyond it a pair's closeness is 0.
pub(crate) const WINDOW: u32 = 5;

/// One pair of adjacent required words, planned: the first one's `must` index (the second is the next), `λ · min(idf)`, and per field with positions
/// holding both, the two terms.
#[derive(Debug, Clone)]
pub(super) struct PairPlan {
    pub(super) first: usize,
    pub(super) weight: f64,
    pub(super) fields: Vec<(FieldId, Vec<u8>, Vec<u8>)>,
}

/// A `bool`'s pairs and the bound of its bonus.
#[derive(Debug, Clone)]
pub(super) struct ProximityPlan {
    pub(super) pairs: Vec<PairPlan>,
    /// `Σ weight`, in pair order: the bonus with every closeness 1.
    pub(super) bound: f64,
}

impl ProximityPlan {
    /// The plan of `pairs`; `None` without one.
    pub(super) fn new(pairs: Vec<PairPlan>) -> Option<Self> {
        if pairs.is_empty() {
            return None;
        }
        let bound = pairs.iter().fold(0.0, |sum, p| sum + p.weight);
        Some(Self { pairs, bound })
    }
}

/// `1 / d²` for `1 ≤ d ≤ WINDOW`, else 0.
pub(crate) fn closeness(d: u32) -> f64 {
    if d == 0 || d > WINDOW {
        return 0.0;
    }
    let d = f64::from(d);
    1.0 / (d * d)
}

/// The least distance between an occurrence in `a` and one in `b` (both ascending): `q − p` for `q > p`, else
/// `p − q + 1`. `same`: the lists are one term's, so an occurrence is not paired with itself. `None` when no pair
/// exists. Stops at 1, the least there is.
pub(crate) fn distance(a: &[u32], b: &[u32], same: bool) -> Option<u32> {
    if same {
        // Adjacent occurrences, in order: `q − p` of the closest two.
        return a
            .windows(2)
            .filter_map(|w| match w {
                [p, q] if q > p => Some(q - p),
                _ => None,
            })
            .min();
    }
    let mut best: Option<u32> = None;
    let mut j = 0;
    for &p in a {
        // `b[j]` is the first after `p`; `b[j - 1]`, if any, the last at or before it.
        while b.get(j).is_some_and(|&q| q <= p) {
            j += 1;
        }
        let after = b.get(j).map(|&q| q - p);
        let before = j
            .checked_sub(1)
            .and_then(|i| b.get(i))
            .map(|&q| (p - q).saturating_add(1));
        for d in [after, before].into_iter().flatten() {
            best = Some(best.map_or(d, |b| b.min(d)));
        }
        if best == Some(1) {
            break;
        }
    }
    best
}

/// One field of a pair over a view: both terms' postings, when the view holds both.
struct PairField<P> {
    field: FieldId,
    first: P,
    second: P,
    same: bool,
    /// The first term's positions at the document, copied to compare with the second's.
    scratch: Vec<u32>,
}

/// One pair over a view.
struct Pair<P> {
    first: usize,
    weight: f64,
    fields: Vec<PairField<P>>,
}

/// A pair's closeness at a document, and where it came from (SPEC §7.7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Nearest {
    /// The field the closeness came from; `None` when no field holds both terms within [`WINDOW`].
    pub(super) field: Option<FieldId>,
    pub(super) distance: Option<u32>,
    pub(super) closeness: f64,
}

/// A `bool`'s bonus over a view: its own postings of the pairs' terms, read only for the documents scored.
pub(super) struct Proximity<P> {
    pairs: Vec<Pair<P>>,
    bound: f64,
}

impl<P: PostingsIterator> Proximity<P> {
    /// The pairs of `plan` over `view`, each field the view holds both terms of.
    pub(super) fn new<'a, V>(plan: &ProximityPlan, view: &'a V) -> Result<Self, ReadError>
    where
        V: SearchView<Postings<'a> = P>,
    {
        let mut pairs = Vec::with_capacity(plan.pairs.len());
        for pair in &plan.pairs {
            let mut fields = Vec::with_capacity(pair.fields.len());
            for (field, first, second) in &pair.fields {
                let (Some(a), Some(b)) = (
                    view.postings(*field, first)?,
                    view.postings(*field, second)?,
                ) else {
                    continue;
                };
                fields.push(PairField {
                    field: *field,
                    first: a,
                    second: b,
                    same: first == second,
                    scratch: Vec::new(),
                });
            }
            pairs.push(Pair {
                first: pair.first,
                weight: pair.weight,
                fields,
            });
        }
        Ok(Self {
            pairs,
            bound: plan.bound,
        })
    }

    /// Per pair, its first word's `must` index, its weight and its closeness at `docid`, as the bonus reads them.
    ///
    /// # Errors
    /// A corruption of the view.
    pub(super) fn explain(&mut self, docid: u32) -> Result<Vec<(usize, f64, Nearest)>, ReadError> {
        (0..self.pairs.len())
            .map(|i| {
                let c = self.closeness(i, docid)?;
                let (first, weight) = self.pairs.get(i).map_or((0, 0.0), |p| (p.first, p.weight));
                Ok((first, weight, c))
            })
            .collect()
    }

    /// Pair `i`'s best field at `docid`: the least distance (first field in query order on a tie).
    fn closeness(&mut self, i: usize, docid: u32) -> Result<Nearest, ReadError> {
        let mut best = Nearest {
            field: None,
            distance: None,
            closeness: 0.0,
        };
        let Some(pair) = self.pairs.get_mut(i) else {
            return Ok(best);
        };
        for f in &mut pair.fields {
            if f.first.advance(docid)? != Some(docid) || f.second.advance(docid)? != Some(docid) {
                continue;
            }
            let d = if f.same {
                distance(f.first.positions()?, &[], true)
            } else {
                // Two iterators: copy one list to compare it with the other's.
                f.scratch.clear();
                f.scratch.extend_from_slice(f.first.positions()?);
                distance(&f.scratch, f.second.positions()?, false)
            };
            if let Some(d) = d
                && d <= WINDOW
                && best.distance.is_none_or(|b| d < b)
            {
                best = Nearest {
                    field: Some(f.field),
                    distance: Some(d),
                    closeness: closeness(d),
                };
            }
        }
        Ok(best)
    }
}

impl<P: PostingsIterator> Bonus for Proximity<P> {
    fn bound(&self) -> f64 {
        self.bound
    }

    /// `Σ weight · closeness`, in pair order.
    fn bonus(&mut self, docid: u32) -> Result<f64, ReadError> {
        let mut sum = 0.0;
        for i in 0..self.pairs.len() {
            let c = self.closeness(i, docid)?;
            if let Some(pair) = self.pairs.get(i) {
                sum += pair.weight * c.closeness;
            }
        }
        Ok(sum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_by_hand() {
        assert_eq!(distance(&[0], &[1], false), Some(1));
        assert_eq!(distance(&[1], &[0], false), Some(2)); // reversed costs one more
        assert_eq!(distance(&[3], &[3], false), Some(1)); // stacked
        assert_eq!(distance(&[0, 10], &[4, 12], false), Some(2));
        assert_eq!(distance(&[0, 10], &[7], false), Some(4)); // 10 − 7 + 1 beats 7 − 0
        assert_eq!(distance(&[2, 5, 9], &[], true), Some(3));
        assert_eq!(distance(&[2], &[], true), None);
        assert_eq!(distance(&[], &[1], false), None);
    }

    #[test]
    fn closeness_is_one_over_d_squared_within_the_window() {
        assert_eq!(closeness(1), 1.0);
        assert_eq!(closeness(2), 0.25);
        assert_eq!(closeness(5), 1.0 / 25.0);
        assert_eq!(closeness(6), 0.0);
        assert_eq!(closeness(0), 0.0);
    }

    /// The bonus never exceeds its bound, bit for bit, whatever the closeness (SPEC §7.3).
    #[test]
    fn the_bonus_is_at_most_its_bound() {
        let weights = [0.0, 1e-300, 0.1, 1.0 / 3.0, 2.75, 17.5, 1e300];
        for &a in &weights {
            for &b in &weights {
                let bound = 0.0 + a + b;
                for &(ca, cb) in &[(1.0, 1.0), (1.0, 0.25), (0.04, 1.0), (0.0, 0.0)] {
                    let bonus = 0.0 + a * ca + b * cb;
                    assert!(bonus <= bound, "{a} {b} {ca} {cb}");
                }
            }
        }
    }
}

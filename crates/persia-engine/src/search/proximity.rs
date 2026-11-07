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

use super::boolean::Bonus;

/// `λ`: an adjacent pair adds `min(idf)` of its two words.
pub(crate) const LAMBDA: f64 = 1.0;

/// The largest distance that counts: beyond it a pair's closeness is 0.
pub(crate) const WINDOW: u32 = 5;

/// One pair of adjacent required words, planned: the first one's `must` index (the second is the next),
/// `λ · min(idf)`, and per field with positions holding both, the two terms.
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

/// The positions a pair reads: a word's, by its `must` index, in a field, at the document the words are on.
pub(super) trait Words {
    /// Appends to `out` the positions of `must` clause `clause` in `field` at the current document (none when the
    /// word is not in that field there).
    ///
    /// # Errors
    /// A corruption of the view.
    fn positions(
        &mut self,
        clause: usize,
        field: FieldId,
        out: &mut Vec<u32>,
    ) -> Result<(), ReadError>;
}

/// A pair's closeness at a document, and where it came from (SPEC §7.7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Nearest {
    /// The field the closeness came from; `None` when no field holds both terms within [`WINDOW`].
    pub(super) field: Option<FieldId>,
    pub(super) distance: Option<u32>,
    pub(super) closeness: f64,
}

/// A `bool`'s bonus: its pairs, read from the positions its words' own postings are on (no postings of its own, so
/// nothing is decoded twice), only for the documents scored.
pub(super) struct Proximity<'p> {
    plan: &'p ProximityPlan,
    /// Scratch: the two words' positions in a field.
    first: Vec<u32>,
    second: Vec<u32>,
}

impl<'p> Proximity<'p> {
    pub(super) fn new(plan: &'p ProximityPlan) -> Self {
        Self {
            plan,
            first: Vec::new(),
            second: Vec::new(),
        }
    }

    /// Per pair, its first word's `must` index, its weight and its closeness at the document `words` are on, as
    /// the bonus reads them.
    ///
    /// # Errors
    /// A corruption of the view.
    pub(super) fn explain(
        &mut self,
        words: &mut dyn Words,
    ) -> Result<Vec<(usize, f64, Nearest)>, ReadError> {
        let plan = self.plan;
        plan.pairs
            .iter()
            .map(|pair| Ok((pair.first, pair.weight, self.nearest(pair, words)?)))
            .collect()
    }

    /// `pair`'s best field: the least distance (the first field in query order on a tie).
    fn nearest(&mut self, pair: &PairPlan, words: &mut dyn Words) -> Result<Nearest, ReadError> {
        let mut best = Nearest {
            field: None,
            distance: None,
            closeness: 0.0,
        };
        let second = pair.first.saturating_add(1);
        for (field, a, b) in &pair.fields {
            self.first.clear();
            words.positions(pair.first, *field, &mut self.first)?;
            if self.first.is_empty() {
                continue;
            }
            let d = if a == b {
                // One term: its own occurrences pair (the second word's list is the same).
                distance(&self.first, &[], true)
            } else {
                self.second.clear();
                words.positions(second, *field, &mut self.second)?;
                distance(&self.first, &self.second, false)
            };
            if let Some(d) = d
                && d <= WINDOW
                && best.distance.is_none_or(|b| d < b)
            {
                best = Nearest {
                    field: Some(*field),
                    distance: Some(d),
                    closeness: closeness(d),
                };
            }
        }
        Ok(best)
    }
}

impl Bonus for Proximity<'_> {
    fn bound(&self) -> f64 {
        self.plan.bound
    }

    /// `Σ weight · closeness`, in pair order.
    fn bonus(&mut self, words: &mut dyn Words) -> Result<f64, ReadError> {
        let plan = self.plan;
        let mut sum = 0.0;
        for pair in &plan.pairs {
            sum += pair.weight * self.nearest(pair, words)?.closeness;
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

    /// Words at fixed positions: clause `i` holds `at[i]` in every field.
    struct Fixed(Vec<Vec<u32>>);

    impl Words for Fixed {
        fn positions(
            &mut self,
            clause: usize,
            _: FieldId,
            out: &mut Vec<u32>,
        ) -> Result<(), ReadError> {
            out.extend_from_slice(self.0.get(clause).map_or(&[][..], Vec::as_slice));
            Ok(())
        }
    }

    /// `Bonus::bonus` against `ProximityPlan::bound`, bit for bit, over many pairs with adversarial weights
    /// (tiny, huge, irrational, the same repeated) and every closeness, adjacent words the worst case (SPEC §7.3).
    #[test]
    fn the_bonus_is_at_most_its_bound() {
        let weights = [
            0.0,
            1e-300,
            0.1,
            1.0 / 3.0,
            2.75,
            17.5,
            1e300,
            f64::MAX / 64.0,
        ];
        let field = FieldId::new(0);
        for n in [1_usize, 2, 7, 40] {
            for offset in 0..weights.len() {
                let pairs: Vec<PairPlan> = (0..n)
                    .map(|i| PairPlan {
                        first: i,
                        weight: weights[(i + offset) % weights.len()],
                        fields: vec![(field, b"a".to_vec(), b"b".to_vec())],
                    })
                    .collect();
                let plan = ProximityPlan::new(pairs).unwrap();
                // Word `i` at position `gap · i`: every pair `gap` apart, in order.
                for gap in 1..=6 {
                    let at = (0..=n)
                        .map(|i| vec![u32::try_from(i).unwrap() * gap])
                        .collect();
                    let bonus = Proximity::new(&plan).bonus(&mut Fixed(at)).unwrap();
                    assert!(bonus <= plan.bound, "{n} pairs, offset {offset}, gap {gap}");
                    if gap == 1 {
                        assert_eq!(
                            bonus.to_bits(),
                            plan.bound.to_bits(),
                            "adjacent: the bound itself"
                        );
                    }
                }
            }
        }
    }
}

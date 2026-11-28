//! Reference static rank (SPEC §6.2.3) and a hit's total (SPEC §7.2, ROADMAP 10.1.45), from the SPEC text alone.

#![allow(dead_code)]

/// `fn` of `ranking.static_rank`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankFn {
    /// `ln(1 + max(v, 0))`.
    Log1p,
    /// `max(v, 0)`.
    Linear,
}

/// `weight · fn(v)` for a document whose field holds `value` (`None`: no value): 0 without a finite value, never
/// negative, and at most `f64::MAX`.
pub fn contribution(weight: f64, function: RankFn, value: Option<f64>) -> f64 {
    let Some(v) = value.filter(|v| v.is_finite()) else {
        return 0.0;
    };
    let v = v.max(0.0);
    let shaped = match function {
        RankFn::Log1p => (1.0 + v).ln(),
        RankFn::Linear => v,
    };
    let product = weight * shaped;
    if product.is_nan() || product <= 0.0 {
        0.0
    } else {
        product.min(f64::MAX)
    }
}

/// A hit's score: its query's score plus the static rank's contribution, at most `f64::MAX` (SPEC §7.2).
pub fn total(query: f64, contribution: f64) -> f64 {
    (query + contribution).min(f64::MAX)
}

//! The static rank contribution `weight · fn(v)` added to every hit's score (SPEC §6.2.3, §7.2, ROADMAP 7.3.2).

use crate::document::Value;
use crate::schema::{FieldType, RankFn, StaticRank};
use crate::segment::KeyBlock;

/// The contribution of a document whose `rank.field` holds `value` (SPEC §6.2.3): `weight · fn(max(v, 0))`, 0 for
/// a document without the field or with a value that is not a finite number, and saturating at `f64::MAX`. Always
/// finite and never negative.
///
/// ```
/// use persia_engine::document::Value;
/// use persia_engine::schema::{RankFn, StaticRank};
/// use persia_engine::score::static_rank;
///
/// let mut rank = StaticRank::new("sales");
/// assert_eq!(static_rank(&rank, Some(&Value::I64(0))), 0.0);
/// assert_eq!(static_rank(&rank, Some(&Value::F64(std::f64::consts::E - 1.0))), 1.0);
/// assert_eq!(static_rank(&rank, Some(&Value::I64(-5))), 0.0);
/// assert_eq!(static_rank(&rank, None), 0.0);
/// rank.function = RankFn::Linear;
/// rank.weight = 0.5;
/// assert_eq!(static_rank(&rank, Some(&Value::I64(10))), 5.0);
/// ```
pub fn static_rank(rank: &StaticRank, value: Option<&Value>) -> f64 {
    let Some(v) = value.and_then(static_rank_value) else {
        return 0.0;
    };
    if !v.is_finite() {
        return 0.0;
    }
    let v = v.max(0.0);
    let shaped = match rank.function {
        RankFn::Log1p => v.ln_1p(),
        RankFn::Linear => v,
    };
    let contribution = rank.weight * shaped;
    if contribution.is_nan() || contribution <= 0.0 {
        // A weight of 0 (or one out of range) contributes nothing.
        0.0
    } else {
        contribution.min(f64::MAX)
    }
}

/// A numeric value as the static rank sees it (SPEC §6.2.3): an `i64` or `f64` as a float, a `datetime` as its
/// microseconds since the epoch; `None` for another value.
// Int to float on purpose: a rank needs magnitude, not the last bits of a 64-bit integer.
#[allow(clippy::cast_precision_loss, clippy::as_conversions)]
pub fn static_rank_value(value: &Value) -> Option<f64> {
    match *value {
        Value::I64(v) | Value::Datetime(v) => Some(v as f64),
        Value::F64(v) => Some(v),
        _ => None,
    }
}

/// The contribution of a document whose key in the static rank's column is `key` (`None`: no value), for a column of
/// `kind` (`i64`, `f64` or `datetime`; SPEC §5.2.1 keys), and the value it came from as [`static_rank_value`] reads
/// it: what L5 ranks by and what every query hit adds (SPEC §6.2.3, §7.2), read from a view's column.
pub(crate) fn key_contribution(
    rank: &StaticRank,
    kind: FieldType,
    key: Option<u64>,
) -> (f64, Option<f64>) {
    let value = key.and_then(|key| {
        let value = match kind {
            FieldType::F64 => Value::F64(persia_format::for_codec::key_to_f64(key)),
            FieldType::Datetime => Value::Datetime(persia_format::for_codec::key_to_i64(key)),
            _ => Value::I64(persia_format::for_codec::key_to_i64(key)),
        };
        static_rank_value(&value)
    });
    let value_ref = value.map(Value::F64);
    // `+ 0.0`: a `-0.0` (a weight of `-0.0`) ranks as `0.0`.
    (static_rank(rank, value_ref.as_ref()) + 0.0, value)
}

/// The largest key `block` may hold, as a bound of its documents' static rank. Its bound is the minimum plus
/// `2^width − 1` (SPEC §5.2.1), which for an `f64` column may pass the key of `f64::MAX` into those of `+∞` and NaN,
/// whose static rank reads as 0; stored floats are finite (SPEC §6.2.2), so the bound stops at `f64::MAX`. The static
/// rank never decreases with the key (key order is value order, and `fn` and the weight are monotone, SPEC §6.2.3),
/// so the static rank of this key bounds every document's in the block.
pub(crate) fn largest_key(kind: FieldType, block: KeyBlock) -> u64 {
    match kind {
        FieldType::F64 => block
            .max
            .min(persia_format::for_codec::f64_to_key(f64::MAX))
            .max(block.min),
        _ => block.max,
    }
}

/// A bound of the static rank of every document of a column no key of which is above `largest` (SPEC §7.3): the
/// static rank of `largest`, which bounds every smaller key's, as for [`largest_key`]. For an `f64` column it stops
/// at the key of `f64::MAX`: the keys above are `+∞` and NaN, whose static rank is 0, and stored floats are finite.
/// A `largest` of 0 (a column without values) is the smallest key of every type, whose static rank is 0.
pub(crate) fn largest_contribution(rank: &StaticRank, kind: FieldType, largest: u64) -> f64 {
    let key = match kind {
        FieldType::F64 => largest.min(persia_format::for_codec::f64_to_key(f64::MAX)),
        _ => largest,
    };
    key_contribution(rank, kind, Some(key)).0
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::*;

    fn rank(function: RankFn, weight: f64) -> StaticRank {
        StaticRank {
            field: "f".into(),
            weight,
            function,
        }
    }

    #[test]
    fn log1p_and_linear() {
        let log = rank(RankFn::Log1p, 2.0);
        assert_eq!(static_rank(&log, Some(&Value::I64(0))), 0.0);
        assert_eq!(
            static_rank(&log, Some(&Value::I64(99))),
            2.0 * 99_f64.ln_1p()
        );
        assert_eq!(
            static_rank(&log, Some(&Value::Datetime(1_000_000))),
            2.0 * 1_000_000_f64.ln_1p()
        );
        let lin = rank(RankFn::Linear, 0.1);
        assert_eq!(static_rank(&lin, Some(&Value::F64(30.0))), 0.1 * 30.0);
    }

    #[test]
    fn negatives_missing_and_other_values_contribute_nothing() {
        for function in [RankFn::Log1p, RankFn::Linear] {
            let r = rank(function, 1.0);
            for v in [
                Value::I64(-1),
                Value::I64(i64::MIN),
                Value::F64(-0.5),
                Value::Datetime(-10),
                Value::F64(f64::NAN),
                Value::F64(f64::INFINITY),
                Value::F64(f64::NEG_INFINITY),
                Value::Bool(true),
                Value::String("7".into()),
            ] {
                assert_eq!(static_rank(&r, Some(&v)), 0.0, "{function:?} {v:?}");
            }
            assert_eq!(static_rank(&r, None), 0.0);
        }
        assert_eq!(
            static_rank(&rank(RankFn::Linear, 0.0), Some(&Value::I64(5))),
            0.0
        );
    }

    #[test]
    fn saturates_finite() {
        let r = rank(RankFn::Linear, 1000.0);
        assert_eq!(static_rank(&r, Some(&Value::F64(f64::MAX))), f64::MAX);
        let r = rank(RankFn::Log1p, 1000.0);
        let v = static_rank(&r, Some(&Value::F64(f64::MAX)));
        assert!(v.is_finite() && v > 0.0);
        let r = rank(RankFn::Linear, 1.0);
        assert_eq!(
            static_rank(&r, Some(&Value::I64(i64::MAX))),
            i64::MAX as f64
        );
    }

    /// The bound of a column by its largest key bounds the static rank of every key up to it, for each type and
    /// function (the contribution never decreases with the key); a column without values (largest key 0) bounds it by
    /// 0, and an `f64` bound past `f64::MAX`'s key (into `+∞` and NaN) stops at `f64::MAX`'s.
    #[test]
    fn the_largest_keys_contribution_bounds_every_smaller_keys() {
        use persia_format::for_codec::{f64_to_key, i64_to_key};
        let mut keys: Vec<u64> = [
            -1e300,
            -2.5,
            -0.0,
            0.0,
            1e-300,
            0.5,
            1.0,
            999.0,
            1e300,
            f64::MAX,
        ]
        .into_iter()
        .map(f64_to_key)
        .chain(
            [i64::MIN, -7, 0, 1, 1_000, i64::MAX]
                .into_iter()
                .map(i64_to_key),
        )
        .chain([0, 1, u64::MAX >> 1, u64::MAX - 1, u64::MAX])
        .collect();
        keys.sort_unstable();
        for kind in [FieldType::I64, FieldType::F64, FieldType::Datetime] {
            for function in [RankFn::Log1p, RankFn::Linear] {
                for weight in [0.001, 1.0, 1000.0] {
                    let r = rank(function, weight);
                    assert_eq!(largest_contribution(&r, kind, 0), 0.0, "{kind:?}");
                    for (i, &largest) in keys.iter().enumerate() {
                        let bound = largest_contribution(&r, kind, largest);
                        for &key in &keys[..=i] {
                            let (s, _) = key_contribution(&r, kind, Some(key));
                            assert!(
                                s <= bound,
                                "{kind:?} {function:?} {weight}: {key:#x} {s} > {largest:#x} {bound}"
                            );
                        }
                    }
                }
            }
        }
        let r = rank(RankFn::Linear, 1.0);
        for past in [f64_to_key(f64::INFINITY), f64_to_key(f64::NAN), u64::MAX] {
            assert_eq!(largest_contribution(&r, FieldType::F64, past), f64::MAX);
        }
    }
}

//! Latency percentiles for `persia bench` (ROADMAP 10.4.3), by the nearest-rank method: the `p`-th percentile of `n`
//! samples is the sample of rank `ceil(p * n / 100)` in ascending order, so it is always a latency that was measured,
//! never an interpolation between two.

/// The latencies `persia bench` reports for one kind of query, in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percentiles {
    /// The median.
    pub p50: u64,
    /// The 90th percentile.
    pub p90: u64,
    /// The 99th percentile.
    pub p99: u64,
    /// The slowest sample.
    pub max: u64,
}

impl Percentiles {
    /// The percentiles of `samples`, in any order; `None` without a sample.
    pub fn of(samples: &[u64]) -> Option<Self> {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        Some(Self {
            p50: nearest_rank(&sorted, 50)?,
            p90: nearest_rank(&sorted, 90)?,
            p99: nearest_rank(&sorted, 99)?,
            max: sorted.last().copied()?,
        })
    }
}

/// The `percent`-th percentile of `sorted` (ascending), by nearest rank; `None` without a sample. A `percent` of 0
/// is the smallest sample, and one of 100 or more the largest.
pub fn nearest_rank(sorted: &[u64], percent: u32) -> Option<u64> {
    let n = u128::try_from(sorted.len()).ok()?;
    // ceil(percent * n / 100), at least 1 and at most n.
    let rank = u128::from(percent)
        .checked_mul(n)?
        .checked_add(99)?
        .checked_div(100)?
        .clamp(1, n.max(1));
    let index = usize::try_from(rank.checked_sub(1)?).ok()?;
    sorted.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_rank_is_a_measured_sample() {
        let samples: Vec<u64> = (1..=10).map(|n| n * 10).collect();
        // ceil(p * 10 / 100): rank 5, 9, 10.
        assert_eq!(nearest_rank(&samples, 50), Some(50));
        assert_eq!(nearest_rank(&samples, 90), Some(90));
        assert_eq!(nearest_rank(&samples, 99), Some(100));
        assert_eq!(nearest_rank(&samples, 91), Some(100));
        assert_eq!(nearest_rank(&samples, 41), Some(50));
        assert_eq!(nearest_rank(&samples, 40), Some(40));
        assert_eq!(nearest_rank(&samples, 1), Some(10));
        assert_eq!(nearest_rank(&samples, 0), Some(10));
        assert_eq!(nearest_rank(&samples, 100), Some(100));
        assert_eq!(nearest_rank(&samples, 250), Some(100));
    }

    #[test]
    fn one_sample_is_every_percentile_and_none_has_none() {
        assert_eq!(nearest_rank(&[7], 50), Some(7));
        assert_eq!(nearest_rank(&[7], 99), Some(7));
        assert_eq!(nearest_rank(&[], 50), None);
        assert_eq!(Percentiles::of(&[]), None);
        assert_eq!(
            Percentiles::of(&[7]),
            Some(Percentiles {
                p50: 7,
                p90: 7,
                p99: 7,
                max: 7
            })
        );
    }

    #[test]
    fn percentiles_sort_their_samples() {
        // 1..=200 shuffled by a fixed stride: rank 100, 180, 198, and the largest.
        let samples: Vec<u64> = (0..200).map(|i| (i * 77) % 200 + 1).collect();
        assert_eq!(
            Percentiles::of(&samples),
            Some(Percentiles {
                p50: 100,
                p90: 180,
                p99: 198,
                max: 200
            })
        );
        // The slowest sample is reported even when p99 is not it.
        let mut spiky = vec![5; 1000];
        spiky.push(9_000_000);
        let found = Percentiles::of(&spiky).unwrap();
        assert_eq!((found.p50, found.p99, found.max), (5, 5, 9_000_000));
    }

    #[test]
    fn a_huge_percent_does_not_overflow() {
        assert_eq!(nearest_rank(&[1, 2, 3], u32::MAX), Some(3));
    }
}

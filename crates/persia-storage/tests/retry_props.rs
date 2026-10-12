//! Property tests for the retry policy (ROADMAP 2.1.3): every delay lies within its cap, caps never shrink and
//! never exceed `max_delay`, and `run` makes at most `max_attempts` calls.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use persia_storage::retry::{Clock, Jitter, RetryPolicy};
use persia_storage::{Error, Op};
use proptest::prelude::*;

#[derive(Debug)]
struct NoSleep;

impl Clock for NoSleep {
    fn sleep(&self, _: Duration) {}
}

fn policy() -> impl Strategy<Value = RetryPolicy> {
    (0u32..10, 0u64..10_000_000_000, 0u64..10_000_000_000).prop_map(|(max_attempts, base, max)| {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::from_nanos(base),
            max_delay: Duration::from_nanos(max),
        }
    })
}

proptest! {
    #[test]
    fn delays_stay_within_monotone_caps(p in policy(), retry in 0u32..100, random in any::<u64>()) {
        let cap = p.delay_cap(retry);
        prop_assert!(cap <= p.max_delay);
        prop_assert!(p.delay_cap(retry + 1) >= cap);
        prop_assert!(p.delay(retry, random) <= cap);
    }

    #[test]
    fn run_calls_at_most_max_attempts(p in policy(), fail_first in 0u32..12, seed in any::<u64>()) {
        let mut calls = 0u32;
        let out = p.run(&NoSleep, &mut Jitter::new(seed), || {
            calls += 1;
            if calls <= fail_first {
                Err(Error::Transient { op: Op::Get, target: "k".into(), source: "503".into() })
            } else {
                Ok(())
            }
        });
        let attempts = p.max_attempts.max(1);
        prop_assert_eq!(calls, (fail_first + 1).min(attempts));
        prop_assert_eq!(out.is_ok(), fail_first < attempts);
    }
}

//! Bounded parallel transfers (ROADMAP 9.3.2, ADR-0050 item 3): `count` jobs on at most `parallelism` scoped threads,
//! one request in flight each, results in index order.

use core::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::error::{Error, Result};

/// Runs `job(0..count)` on at most `parallelism` threads (the caller's included) and returns the results in index
/// order, or the error of the lowest failing index. After a failure no new index starts; every index below a failure
/// was started before it, so the error reported does not depend on scheduling.
pub(crate) fn run<T, F>(count: usize, parallelism: NonZeroUsize, job: F) -> Result<Vec<T>>
where
    T: Send,
    F: Fn(usize) -> Result<T> + Sync,
{
    let workers = parallelism.get().min(count);
    if workers <= 1 {
        return (0..count).map(job).collect();
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let slots: Vec<Mutex<Option<Result<T>>>> = (0..count).map(|_| Mutex::new(None)).collect();
    let work = || {
        while !failed.load(Ordering::Acquire) {
            let index = next.fetch_add(1, Ordering::AcqRel);
            let Some(slot) = slots.get(index) else {
                break;
            };
            let result = job(index);
            if result.is_err() {
                failed.store(true, Ordering::Release);
            }
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..workers {
            scope.spawn(work);
        }
        work();
    });
    let mut out = Vec::with_capacity(count);
    for slot in slots {
        match slot.into_inner().unwrap_or_else(PoisonError::into_inner) {
            Some(Ok(value)) => out.push(value),
            Some(Err(e)) => return Err(e),
            // Not started: only after a failure at a lower index, which returned above.
            None => break,
        }
    }
    if out.len() == count {
        Ok(out)
    } else {
        Err(Error::Corrupt {
            encoding: "blob chunk",
            what: "a parallel transfer lost a result",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn n(v: usize) -> NonZeroUsize {
        NonZeroUsize::new(v).unwrap()
    }

    #[test]
    fn results_come_back_in_index_order() {
        for p in [1, 2, 3, 8, 64] {
            for count in [0, 1, 2, 7, 100] {
                let out = run(count, n(p), |i| Ok(i * 3)).unwrap();
                assert_eq!(
                    out,
                    (0..count).map(|i| i * 3).collect::<Vec<_>>(),
                    "{p} {count}"
                );
            }
        }
    }

    #[test]
    fn the_lowest_failing_index_is_reported_and_no_more_start() {
        for p in [1, 2, 4, 16] {
            let started = AtomicUsize::new(0);
            let err = run(1000, n(p), |i| {
                started.fetch_add(1, Ordering::Relaxed);
                if i == 10 || i == 11 || i == 500 {
                    Err(Error::InvalidResume {
                        what: if i == 10 { "ten" } else { "other" },
                    })
                } else {
                    Ok(i)
                }
            })
            .unwrap_err();
            assert!(
                matches!(err, Error::InvalidResume { what: "ten" }),
                "{p}: {err:?}"
            );
            // At most `p` jobs were in flight when the failure was seen.
            assert!(started.load(Ordering::Relaxed) <= 11 + p, "{p}");
        }
    }

    #[test]
    fn at_most_parallelism_jobs_run_at_once_and_that_many_do() {
        use std::sync::{Condvar, Mutex};
        for p in [2, 3, 5] {
            let state = Mutex::new((0_usize, 0_usize)); // (in flight, started)
            let changed = Condvar::new();
            let max = AtomicUsize::new(0);
            let threads = Mutex::new(std::collections::BTreeSet::new());
            run(40, n(p), |_| {
                threads
                    .lock()
                    .unwrap()
                    .insert(format!("{:?}", std::thread::current().id()));
                let mut s = state.lock().unwrap();
                s.0 += 1;
                s.1 += 1;
                max.fetch_max(s.0, Ordering::Relaxed);
                changed.notify_all();
                // Wait (bounded) until `p` jobs are in flight together or every job has started, so the run shows real
                // overlap, and an unbounded run would pile up more.
                let (mut s, _) = changed
                    .wait_timeout_while(s, std::time::Duration::from_secs(10), |s| {
                        s.0 < p && s.1 < 40
                    })
                    .unwrap();
                s.0 -= 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(max.load(Ordering::Relaxed), p);
            assert_eq!(
                threads.lock().unwrap().len(),
                p,
                "jobs ran on exactly `p` threads"
            );
        }
    }
}

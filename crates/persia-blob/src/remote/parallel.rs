//! Bounded parallel transfers (ROADMAP 9.3.2, ADR-0050 item 3): `count` jobs on at most `parallelism` scoped threads,
//! one request in flight each, results in index order.

use core::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::error::{Error, Result};

/// Runs `job(0..count)` on at most `parallelism` threads (the caller's included) and returns the results in index
/// order, or the error of the lowest failing index. Once a thread sees a failure it starts no new index (a thread
/// that checked just before the failure was recorded may start one more, whose result is dropped); every index below
/// a failure was started before it, so the error reported does not depend on scheduling.
pub(crate) fn run<T, F>(count: usize, parallelism: NonZeroUsize, job: F) -> Result<Vec<T>>
where
    T: Send,
    F: Fn(usize) -> Result<T> + Sync,
{
    run_with(count, parallelism, None, job)
}

/// [`run`], with the spawned threads' stack size (`None`: the default). A thread that fails to spawn (no memory, a
/// thread limit) is skipped: the jobs run on the threads that did start, the caller's at least. Tests ask for an
/// impossible stack size to make every spawn fail.
fn run_with<T, F>(
    count: usize,
    parallelism: NonZeroUsize,
    stack_size: Option<usize>,
    job: F,
) -> Result<Vec<T>>
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
            let builder = stack_size.map_or_else(std::thread::Builder::new, |size| {
                std::thread::Builder::new().stack_size(size)
            });
            // Fewer threads only slow the transfer down: the caller's thread below takes every index left.
            if builder.spawn_scoped(scope, work).is_err() {
                break;
            }
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
    #[cfg(target_pointer_width = "64")]
    fn threads_that_fail_to_spawn_leave_the_jobs_to_the_caller() {
        // No 64-bit platform maps a 2^62-byte stack, so every spawn fails, deterministically.
        let caller = format!("{:?}", std::thread::current().id());
        let threads = Mutex::new(std::collections::BTreeSet::new());
        let out = run_with(50, n(8), Some(1 << 62), |i| {
            threads
                .lock()
                .unwrap()
                .insert(format!("{:?}", std::thread::current().id()));
            Ok(i)
        })
        .unwrap();
        assert_eq!(out, (0..50).collect::<Vec<_>>());
        assert_eq!(
            threads
                .into_inner()
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            [caller]
        );
        let err = run_with(50, n(8), Some(1 << 62), |i| {
            if i == 7 {
                Err(Error::InvalidResume { what: "seven" })
            } else {
                Ok(i)
            }
        })
        .unwrap_err();
        assert!(matches!(err, Error::InvalidResume { what: "seven" }));
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

    /// Runs 1000 jobs on `p` threads where 10, 11 and 500 fail; returns the error and how many jobs started.
    fn run_failing_at_10_11_500(p: usize) -> (Error, usize) {
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
        (err, started.load(Ordering::Relaxed))
    }

    #[test]
    fn the_lowest_failing_index_is_reported() {
        // Every index below a failure starts before it, so this holds under any scheduling (Miri's included).
        for p in [1, 2, 4, 16] {
            let (err, _) = run_failing_at_10_11_500(p);
            assert!(
                matches!(err, Error::InvalidResume { what: "ten" }),
                "{p}: {err:?}"
            );
        }
    }

    #[test]
    fn few_jobs_start_after_a_failure_under_native_scheduling() {
        // At most `p` jobs were in flight when the failure was seen. The bound depends on scheduling: the failure is
        // recorded only after the failing job returns, so a thread paused between the two lets the others start any
        // number of indices. Native threads close that gap in nanoseconds; Miri pauses threads anywhere, so `just
        // miri` skips this test. The scheduling-independent proof is ROADMAP 35.3.3 (Loom).
        for p in [1, 2, 4, 16] {
            let (err, started) = run_failing_at_10_11_500(p);
            assert!(
                matches!(err, Error::InvalidResume { what: "ten" }),
                "{p}: {err:?}"
            );
            assert!(started <= 11 + p, "{p}: {started} started");
        }
    }

    #[test]
    fn a_panicking_job_unwinds_out_of_run() {
        // A job calls the caller's `Storage`, which may panic. The other workers must still finish, so the scope
        // joins them and the panic reaches the caller, rather than the call waiting forever on the job that never
        // finished. More jobs than threads, so a worker is always left to claim after the panic.
        for p in [2, 4, 8] {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(64, n(p), |i| {
                    assert!(i != 3, "job three panics");
                    Ok(i)
                })
            }));
            assert!(outcome.is_err(), "{p}: the panic did not reach the caller");
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

//! Bounded parallel transfers (ROADMAP 9.3.2, ADR-0050 item 3): `count` jobs on at most `parallelism` scoped threads,
//! one request in flight each, results in index order.

use core::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::error::{Error, Result};

/// `state` value: the index has finished and its job returned `Ok`.
const FINISHED_OK: u8 = 1;
/// `state` value: the index has finished and its job returned `Err`. Stored only after `failed` is set.
const FINISHED_ERR: u8 = 2;

/// Runs `job(0..count)` on at most `parallelism` threads (the caller's included) and returns the results in index
/// order, or the error of the lowest failing index. A worker claims the next index only after it has seen no failure,
/// and only while `index < finished_prefix + parallelism`. The prefix is the longest leading run of finished indices.
/// It does not move past a failure until that failure is published, so a failure at `f` starts at most `f + parallelism`
/// jobs (indices `0..f + parallelism`): the `parallelism` slots of the window stuck on `f`. A worker that checked just
/// before the failure was recorded may claim one more index, still inside that window, and its result is dropped.
/// Every index below a failure was started before it, so the error reported does not depend on scheduling.
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
    // Longest leading run of finished indices. Claims stay below `prefix + workers`, so a failure that has not
    // finished yet stops the run `workers` indices past it, instead of letting the other workers race to `count`.
    let prefix = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<Result<T>>>> = (0..count).map(|_| Mutex::new(None)).collect();
    // `0` pending, [`FINISHED_OK`], or [`FINISHED_ERR`] (the last only after `failed` is set).
    let state: Vec<AtomicU8> = (0..count).map(|_| AtomicU8::new(0)).collect();
    let work = || {
        loop {
            if failed.load(Ordering::Acquire) {
                break;
            }
            let at = prefix.load(Ordering::Acquire);
            let index = next.load(Ordering::Acquire);
            if index >= count {
                break;
            }
            if index >= at.saturating_add(workers) {
                // The window is full. Try the prefix again: the index that held it may have finished since
                // this thread last looked, and that store is visible now even if an earlier advance missed it.
                advance_prefix(&prefix, &state, &failed);
                if prefix.load(Ordering::Acquire).saturating_add(workers) <= index
                    && !failed.load(Ordering::Acquire)
                {
                    std::thread::yield_now();
                }
                continue;
            }
            // Immediately before the claim. A prefix past a failure is published only after `failed`, and this
            // load is sequenced after the prefix load, so it sees that flag.
            if failed.load(Ordering::Acquire) {
                break;
            }
            let Some(claimed) = index.checked_add(1) else {
                break;
            };
            if next
                .compare_exchange(index, claimed, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            let Some(slot) = slots.get(index) else {
                break;
            };
            let result = job(index);
            let err = result.is_err();
            if err {
                failed.store(true, Ordering::Release);
            }
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
            if let Some(cell) = state.get(index) {
                cell.store(
                    if err { FINISHED_ERR } else { FINISHED_OK },
                    Ordering::Release,
                );
            }
            advance_prefix(&prefix, &state, &failed);
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

/// Moves `prefix` over the finished leading run. A failure is stored again before the prefix steps past it, so a
/// claimer that observes the new prefix (and then loads `failed`) also observes the flag.
fn advance_prefix(prefix: &AtomicUsize, state: &[AtomicU8], failed: &AtomicBool) {
    loop {
        let at = prefix.load(Ordering::Acquire);
        let kind = match state.get(at) {
            Some(cell) => cell.load(Ordering::Acquire),
            None => break,
        };
        if kind == FINISHED_ERR {
            failed.store(true, Ordering::Release);
        } else if kind != FINISHED_OK {
            break;
        }
        let Some(next_at) = at.checked_add(1) else {
            break;
        };
        // Success steps one index ahead. A lost race reloads the prefix another worker published.
        let _ = prefix.compare_exchange(at, next_at, Ordering::AcqRel, Ordering::Acquire);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

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
            // `run`: a claim happens only after the worker has seen no failure, and only while
            // `index < finished_prefix + parallelism`. The prefix does not move past a failure until
            // that failure is published, so with the lowest failure at 10 the highest index started
            // is `10 + p - 1` (the window stuck on 10): at most `10 + p` starts. Indices `0..=10`
            // all ran, which is why the error is the one from 10.
            let nstarted = started.load(Ordering::Relaxed);
            assert!((11..=10 + p).contains(&nstarted), "{p}: {nstarted}");
        }
    }

    #[test]
    fn at_most_parallelism_jobs_run_at_once_and_that_many_do() {
        use std::sync::{Condvar, Mutex};
        for p in [2, 3, 5] {
            let state = Mutex::new((0_usize, 0_usize)); // (in flight, started)
            let changed = Condvar::new();
            let max = AtomicUsize::new(0);
            // Sticky: set once `p` jobs are in flight together. A fast job may leave before a slow one
            // rechecks the count, and `run` will not start a later index while an older one is unfinished.
            let full = AtomicBool::new(false);
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
                if s.0 == p {
                    full.store(true, Ordering::Release);
                }
                changed.notify_all();
                // Wait (bounded) until `p` jobs are in flight together or every job has started, so the run shows real
                // overlap, and an unbounded run would pile up more.
                let (mut s, _) = changed
                    .wait_timeout_while(s, std::time::Duration::from_secs(10), |s| {
                        !full.load(Ordering::Acquire) && s.1 < 40
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

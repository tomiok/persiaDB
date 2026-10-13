//! `FaultyStorage` (ROADMAP 2.2.2): each fault behaves as documented, schedules replay from their seed, and with no
//! faults armed the wrapper passes the conformance suite.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

#[macro_use]
mod conformance;

use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use persia_storage::faulty::{Fault, FaultyStorage, Injection, RandomFaults, Trigger};
use persia_storage::retry::{Clock, Jitter, RetryPolicy};
use persia_storage::{ErrorKind, Key, KeyPrefix, MemoryStorage, Op, PutOutcome, Storage};
use proptest::prelude::*;

mod transparent {
    use persia_storage::MemoryStorage;
    use persia_storage::faulty::FaultyStorage;

    storage_conformance!(FaultyStorage::new(MemoryStorage::new(), 0));
}

#[derive(Debug, Default, Clone)]
struct RecordingClock(Arc<Mutex<Vec<Duration>>>);

impl Clock for RecordingClock {
    fn sleep(&self, duration: Duration) {
        self.0.lock().unwrap().push(duration);
    }
}

fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

fn faulty(seed: u64) -> FaultyStorage<MemoryStorage> {
    FaultyStorage::new(MemoryStorage::new(), seed)
}

fn data() -> Bytes {
    Bytes::from_static(b"segment bytes, 32 of them.......")
}

#[test]
fn error_faults_fail_the_nth_call_without_applying_it() {
    for (fault, kind) in [
        (Fault::Transient, ErrorKind::Transient),
        (Fault::Io, ErrorKind::Io),
    ] {
        let s = faulty(1);
        s.inject(Trigger::Call(2), fault);
        s.put(&key("a"), data()).unwrap();
        assert_eq!(s.put(&key("b"), data()).unwrap_err().kind(), kind);
        assert_eq!(
            s.inner().get(&key("b")).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        s.put(&key("c"), data()).unwrap();
        assert_eq!(s.calls(), 3);
        assert_eq!(
            s.injected(),
            [Injection {
                call: 2,
                op: Op::Put,
                target: "b".into(),
                fault,
                kept: None
            }]
        );
    }
}

#[test]
fn nth_counts_calls_of_one_operation() {
    let s = faulty(1);
    s.inner().put(&key("k"), data()).unwrap();
    s.inject(Trigger::Nth(Op::Get, 3), Fault::Transient);
    for _ in 0..4 {
        s.list(&KeyPrefix::new("").unwrap()).unwrap();
    }
    s.get(&key("k")).unwrap();
    s.get(&key("k")).unwrap();
    assert_eq!(s.get(&key("k")).unwrap_err().kind(), ErrorKind::Transient);
    s.get(&key("k")).unwrap();
    assert_eq!(s.injected()[0].call, 7);
}

#[test]
fn every_operation_can_fail() {
    let s = faulty(1);
    let k = key("k");
    s.inner().put(&k, data()).unwrap();
    for n in 1..=6 {
        s.inject(Trigger::Call(n), Fault::Transient);
    }
    let kinds = [
        s.get(&k).unwrap_err().kind(),
        s.get_range(&k, 0..1).unwrap_err().kind(),
        s.put(&k, data()).unwrap_err().kind(),
        s.put_if_absent(&k, data()).unwrap_err().kind(),
        s.list(&KeyPrefix::new("").unwrap()).unwrap_err().kind(),
        s.delete(&k).unwrap_err().kind(),
    ];
    assert_eq!(kinds, [ErrorKind::Transient; 6]);
    assert_eq!(
        s.inner().get(&k).unwrap(),
        data(),
        "the failed delete was not applied"
    );
}

#[test]
fn latency_sleeps_on_the_clock_then_proceeds() {
    let clock = RecordingClock::default();
    let s = faulty(1).with_clock(clock.clone());
    s.inject(
        Trigger::Nth(Op::Put, 1),
        Fault::Latency(Duration::from_millis(30)),
    );
    s.put(&key("k"), data()).unwrap();
    assert_eq!(s.get(&key("k")).unwrap(), data());
    assert_eq!(*clock.0.lock().unwrap(), [Duration::from_millis(30)]);
}

#[test]
fn torn_put_writes_a_strict_prefix_and_fails() {
    for seed in 0..50 {
        let s = faulty(seed);
        s.inject(Trigger::Call(1), Fault::TornPut);
        let err = s.put(&key("k"), data()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Io, "seed {seed}");
        let kept = s.injected()[0].kept.unwrap();
        let stored = s.inner().get(&key("k")).unwrap();
        assert!(kept < data().len(), "seed {seed}");
        assert_eq!(stored, data().slice(..kept), "seed {seed}");
    }
    // An empty write has no strict prefix: it is written whole, and still fails.
    let s = faulty(0);
    s.inject(Trigger::Call(1), Fault::TornPut);
    s.put(&key("k"), Bytes::new()).unwrap_err();
    assert_eq!(s.inner().get(&key("k")).unwrap(), Bytes::new());
}

#[test]
fn put_if_absent_is_never_torn() {
    // Every backend's put_if_absent is atomic (SPEC §10.1), so a torn one is a state no store can reach.
    let s = faulty(3);
    s.inject(Trigger::Nth(Op::PutIfAbsent, 1), Fault::TornPut);
    assert_eq!(
        s.put_if_absent(&key("m"), data()).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(s.inner().get(&key("m")).unwrap(), data());
    assert_eq!((s.injected(), s.pending()), (vec![], vec![]));
}

#[test]
fn dropped_put_if_absent_still_reports_another_writers_object() {
    let s = faulty(1);
    let k = key("manifests/00000001");
    let theirs = Bytes::from_static(b"manifest by writer B");
    s.inner().put(&k, theirs.clone()).unwrap();
    s.inject(Trigger::Nth(Op::PutIfAbsent, 1), Fault::DroppedPut);
    s.inject(Trigger::Nth(Op::PutIfAbsent, 2), Fault::DroppedPut);
    assert_eq!(
        s.put_if_absent(&k, data()).unwrap(),
        PutOutcome::AlreadyExists,
        "fencing holds"
    );
    assert_eq!(
        s.put_if_absent(&k, theirs.clone()).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(s.inner().get(&k).unwrap(), theirs);
    assert_eq!(s.injected().len(), 2);
}

#[test]
fn pending_lists_faults_that_have_not_fired() {
    let s = faulty(1);
    s.inject(Trigger::Call(2), Fault::Io);
    s.inject(Trigger::Nth(Op::Delete, 1), Fault::Io);
    s.get(&key("k")).unwrap_err();
    assert_eq!(
        s.pending(),
        [
            (Trigger::Call(2), Fault::Io),
            (Trigger::Nth(Op::Delete, 1), Fault::Io)
        ]
    );
    s.get(&key("k")).unwrap_err();
    assert_eq!(s.pending(), [(Trigger::Nth(Op::Delete, 1), Fault::Io)]);
}

#[test]
fn a_shared_inner_store_survives_the_wrapper() {
    let store = Arc::new(MemoryStorage::new());
    let s = FaultyStorage::new(Arc::clone(&store), 1);
    s.inject(Trigger::Nth(Op::Put, 2), Fault::Io);
    s.put(&key("a"), data()).unwrap();
    s.put(&key("b"), data()).unwrap_err();
    drop(s);
    // "Reopen" on what survived, through the trait object the engine will hold.
    let reopened: Box<dyn Storage> = Box::new(store);
    assert_eq!(reopened.get(&key("a")).unwrap(), data());
    assert_eq!(
        reopened.get(&key("b")).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert!(reopened.capabilities().conditional_put);
    let s = FaultyStorage::new(MemoryStorage::new(), 2);
    s.put(&key("c"), data()).unwrap();
    assert_eq!(s.into_inner().get(&key("c")).unwrap(), data());
}

#[test]
fn dropped_put_is_acknowledged_but_never_applied() {
    let s = faulty(1);
    s.inject(Trigger::Nth(Op::Put, 1), Fault::DroppedPut);
    s.inject(Trigger::Nth(Op::PutIfAbsent, 1), Fault::DroppedPut);
    s.put(&key("a"), data()).unwrap();
    assert_eq!(
        s.put_if_absent(&key("b"), data()).unwrap(),
        PutOutcome::Created
    );
    for k in ["a", "b"] {
        assert_eq!(s.get(&key(k)).unwrap_err().kind(), ErrorKind::NotFound);
    }
}

#[test]
fn response_lost_applies_then_reports_transient() {
    let s = faulty(1);
    let k = key("k");
    s.inner().put(&k, data()).unwrap();
    for op in [Op::Get, Op::Delete] {
        s.inject(Trigger::Nth(op, 1), Fault::ResponseLost);
    }
    assert_eq!(s.get(&k).unwrap_err().kind(), ErrorKind::Transient);
    assert_eq!(s.delete(&k).unwrap_err().kind(), ErrorKind::Transient);
    assert_eq!(
        s.inner().get(&k).unwrap_err().kind(),
        ErrorKind::NotFound,
        "delete applied"
    );
    for (op, k) in [(Op::Put, "p"), (Op::PutIfAbsent, "q")] {
        s.inject(Trigger::Nth(op, 1), Fault::ResponseLost);
        let err = if op == Op::Put {
            s.put(&key(k), data()).unwrap_err()
        } else {
            s.put_if_absent(&key(k), data()).unwrap_err()
        };
        assert_eq!(err.kind(), ErrorKind::Transient, "{op}");
        assert_eq!(s.inner().get(&key(k)).unwrap(), data(), "{op} applied");
    }
}

#[test]
fn write_faults_on_reads_are_discarded() {
    let s = faulty(1);
    s.inner().put(&key("k"), data()).unwrap();
    s.inject(Trigger::Call(1), Fault::TornPut);
    s.get(&key("k")).unwrap();
    // Discarded, not re-armed for a later write.
    s.put(&key("k"), data()).unwrap();
    assert_eq!(s.injected(), []);
    assert!(!Fault::DroppedPut.applies_to(Op::Delete));
    assert!(Fault::ResponseLost.applies_to(Op::List));
}

/// SPEC §10.1, ROADMAP 2.2.2.3: the first `put_if_absent` lands but its response is lost; the retry finds the
/// caller's own write and reports `Created`, not a lost commit.
#[test]
fn retried_put_if_absent_finding_its_own_write_is_created() {
    let clock = RecordingClock::default();
    let s = faulty(1);
    let k = key("manifests/00000001");
    let mine = Bytes::from_static(b"manifest by writer A, fence 7");
    s.inject(Trigger::Nth(Op::PutIfAbsent, 1), Fault::ResponseLost);
    let outcome = RetryPolicy::default()
        .run_put_if_absent(
            &clock,
            &Jitter::new(1),
            &mine,
            || s.put_if_absent(&k, mine.clone()),
            || s.get(&k),
        )
        .unwrap();
    assert_eq!(outcome, PutOutcome::Created);
    assert_eq!(
        s.calls(),
        2,
        "one retry, no read-back needed: the backend resolved it"
    );
    assert_eq!(clock.0.lock().unwrap().len(), 1);

    // Another writer's object is still a lost race.
    s.inject(Trigger::Nth(Op::PutIfAbsent, 2), Fault::ResponseLost);
    let k2 = key("manifests/00000002");
    s.inner()
        .put(&k2, Bytes::from_static(b"manifest by writer B, fence 8"))
        .unwrap();
    let outcome = RetryPolicy::default()
        .run_put_if_absent(
            &clock,
            &Jitter::new(1),
            &mine,
            || s.put_if_absent(&k2, mine.clone()),
            || s.get(&k2),
        )
        .unwrap();
    assert_eq!(outcome, PutOutcome::AlreadyExists);
}

/// Every call of a fixed workload, as `(op, ok?)`, plus the injection log.
fn run_workload(s: &FaultyStorage<MemoryStorage>) -> (Vec<(Op, bool)>, Vec<Injection>) {
    let mut seen = Vec::new();
    for i in 0..200u32 {
        let k = key(&format!("k{}", i % 7));
        let (op, ok) = match i % 4 {
            0 => (
                Op::Put,
                s.put(&k, Bytes::from(vec![1; 1 + i as usize % 40])).is_ok(),
            ),
            1 => (
                Op::PutIfAbsent,
                s.put_if_absent(&k, Bytes::from(vec![2; 9])).is_ok(),
            ),
            2 => (Op::Get, s.get(&k).is_ok()),
            _ => (Op::Delete, s.delete(&k).is_ok()),
        };
        seen.push((op, ok));
    }
    (seen, s.injected())
}

fn random_faults(one_in: u64) -> RandomFaults {
    RandomFaults {
        one_in: NonZeroU64::new(one_in).unwrap(),
        faults: vec![
            Fault::Transient,
            Fault::Io,
            Fault::TornPut,
            Fault::DroppedPut,
            Fault::ResponseLost,
            Fault::Latency(Duration::from_millis(5)),
        ],
    }
}

proptest! {
    /// ROADMAP 2.2.2.2: the same seed and calls give the same faults, torn lengths and outcomes.
    #[test]
    fn random_schedules_replay_from_their_seed(seed in any::<u64>(), one_in in 1..8u64) {
        let run = || {
            let s = faulty(seed).with_clock(RecordingClock::default());
            s.set_random(Some(random_faults(one_in)));
            run_workload(&s)
        };
        let (a, b) = (run(), run());
        prop_assert_eq!(&a, &b, "seed {}", seed);
        prop_assert!(a.1.iter().all(|inj| inj.fault.applies_to(inj.op)));
    }
}

#[test]
fn different_seeds_give_different_schedules() {
    let schedule = |seed| {
        let s = faulty(seed).with_clock(RecordingClock::default());
        s.set_random(Some(random_faults(3)));
        run_workload(&s).1
    };
    let first = schedule(0);
    assert!(
        (1..5).any(|seed| schedule(seed) != first),
        "the schedule ignores the seed"
    );
}

#[test]
fn random_faults_hit_about_one_call_in_n_and_stop_when_disarmed() {
    let s = faulty(42).with_clock(RecordingClock::default());
    s.set_random(Some(random_faults(4)));
    let (_, log) = run_workload(&s);
    assert!(
        (25..=75).contains(&log.len()),
        "{} faults in 200 calls",
        log.len()
    );
    let faults: std::collections::HashSet<Fault> = log.iter().map(|i| i.fault).collect();
    assert!(faults.len() >= 5, "{faults:?}");
    s.set_random(None);
    let (_, after) = run_workload(&s);
    assert_eq!(after.len(), log.len());
    assert_eq!(faulty(1).seed(), 1);
}

#[test]
fn scripted_faults_take_precedence_over_random_ones() {
    let s = faulty(5);
    s.set_random(Some(RandomFaults {
        one_in: NonZeroU64::MIN,
        faults: vec![Fault::Io],
    }));
    s.inject(Trigger::Call(1), Fault::Transient);
    assert_eq!(s.get(&key("k")).unwrap_err().kind(), ErrorKind::Transient);
    assert_eq!(s.get(&key("k")).unwrap_err().kind(), ErrorKind::Io);
    // No applicable fault to choose from: the call proceeds.
    s.set_random(Some(RandomFaults {
        one_in: NonZeroU64::MIN,
        faults: vec![Fault::DroppedPut],
    }));
    assert_eq!(s.get(&key("k")).unwrap_err().kind(), ErrorKind::NotFound);
}

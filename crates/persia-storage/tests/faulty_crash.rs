//! `FaultyStorage` crash modelling (ROADMAP 2.2.4): a sticky crash stops every later call, and in volatile mode a
//! crash loses unsynced writes as a suffix, or per key, exactly as `lost()` reports.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use bytes::Bytes;
use persia_storage::faulty::{CrashLoss, Fault, FaultyStorage, RandomFaults, Trigger};
use persia_storage::retry::Clock;
use persia_storage::{ErrorKind, Key, KeyPrefix, MemoryStorage, Op, PutOutcome, Storage};
use proptest::prelude::*;

fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

fn bytes(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

fn faulty(seed: u64) -> FaultyStorage<MemoryStorage> {
    FaultyStorage::new(MemoryStorage::new(), seed)
}

fn every_call_fails(s: &FaultyStorage<MemoryStorage>) {
    let k = key("k");
    let kinds = [
        s.get(&k).unwrap_err().kind(),
        s.get_range(&k, 0..0).unwrap_err().kind(),
        s.put(&k, bytes("x")).unwrap_err().kind(),
        s.put_if_absent(&k, bytes("x")).unwrap_err().kind(),
        s.list(&KeyPrefix::new("").unwrap()).unwrap_err().kind(),
        s.delete(&k).unwrap_err().kind(),
    ];
    assert_eq!(kinds, [ErrorKind::Io; 6]);
}

#[test]
fn a_crash_fails_its_call_and_every_later_one() {
    let s = faulty(1);
    s.inject(Trigger::Call(3), Fault::Crash);
    s.put(&key("a"), bytes("1")).unwrap();
    s.put(&key("b"), bytes("2")).unwrap();
    let err = s.put(&key("c"), bytes("3")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Io);
    assert!(!err.is_retryable(), "a retry loop stops at a crash");
    assert!(s.is_crashed());
    every_call_fails(&s);
    assert_eq!(
        s.injected().len(),
        1,
        "logged once, for the call that crashed"
    );
    // Nothing reached the store after the crash; what was written before survives (nothing volatile).
    let survived: Vec<String> = s
        .inner()
        .list(&KeyPrefix::new("").unwrap())
        .unwrap()
        .into_iter()
        .map(|m| m.key.as_str().to_owned())
        .collect();
    assert_eq!(survived, ["a", "b"]);
}

#[test]
fn faults_armed_for_after_the_crash_do_not_fire() {
    let s = faulty(1);
    s.inject(Trigger::Nth(Op::Get, 1), Fault::Crash);
    s.inject(Trigger::Call(2), Fault::Transient);
    s.get(&key("k")).unwrap_err();
    assert_eq!(s.get(&key("k")).unwrap_err().kind(), ErrorKind::Io);
    assert_eq!(s.pending(), [(Trigger::Call(2), Fault::Transient)]);
}

#[test]
fn restart_comes_back_on_what_survived() {
    let s = faulty(1);
    s.put(&key("a"), bytes("1")).unwrap();
    s.crash().unwrap();
    s.crash().unwrap();
    every_call_fails(&s);
    s.restart();
    assert!(!s.is_crashed());
    assert_eq!(s.get(&key("a")).unwrap(), bytes("1"));
    s.put(&key("b"), bytes("2")).unwrap();
    assert!(s.injected().is_empty(), "crash() is not an injected fault");
}

#[test]
fn a_random_schedule_can_crash() {
    let s = faulty(9);
    s.set_random(Some(RandomFaults {
        one_in: NonZeroU64::new(20).unwrap(),
        faults: vec![Fault::Crash],
    }));
    let mut first_failure = None;
    for i in 0..1000u32 {
        if s.put(&key("k"), Bytes::from(i.to_le_bytes().to_vec()))
            .is_err()
        {
            first_failure.get_or_insert(i);
        }
    }
    let first = first_failure.expect("crashes within 1000 calls at 1 in 20");
    assert!(s.is_crashed());
    assert_eq!(
        s.injected().len(),
        1,
        "after the crash no call is decided again"
    );
    assert_eq!(
        s.inner().get(&key("k")).unwrap(),
        Bytes::from((first - 1).to_le_bytes().to_vec()),
        "the last write before the crash is what survives"
    );
}

#[test]
fn unsynced_writes_are_visible_until_the_crash() {
    for seed in 0..20 {
        let s = faulty(seed);
        s.set_volatile(Some(CrashLoss::Suffix));
        s.put(&key("a"), bytes("1")).unwrap();
        assert_eq!(s.get(&key("a")).unwrap(), bytes("1"), "read your writes");
        s.sync();
        s.put(&key("a"), bytes("2")).unwrap();
        s.delete(&key("a")).unwrap();
        s.crash().unwrap();
        let survived = s.inner().get(&key("a")).ok();
        let lost = s.lost().len();
        let expected = [None, Some(bytes("2")), Some(bytes("1"))][lost].clone();
        assert_eq!(survived, expected, "seed {seed}: synced write never lost");
    }
}

#[test]
fn durable_mode_loses_nothing_and_turning_it_on_syncs() {
    let s = faulty(3);
    s.set_volatile(Some(CrashLoss::Suffix));
    s.put(&key("a"), bytes("1")).unwrap();
    s.set_volatile(None);
    s.put(&key("b"), bytes("2")).unwrap();
    s.crash().unwrap();
    assert_eq!(s.lost(), []);
    assert_eq!(s.inner().get(&key("a")).unwrap(), bytes("1"));
    assert_eq!(s.inner().get(&key("b")).unwrap(), bytes("2"));
}

#[test]
fn faulted_writes_that_changed_the_object_can_be_lost_too() {
    // A torn put and an applied put_if_absent whose response was lost both changed the store: a crash may undo
    // them like any acknowledged write. Over seeds, both outcomes occur.
    let mut outcomes = std::collections::BTreeSet::new();
    for seed in 0..40 {
        let s = faulty(seed);
        s.set_volatile(Some(CrashLoss::PerKey));
        s.inject(Trigger::Nth(Op::Put, 1), Fault::TornPut);
        s.inject(Trigger::Nth(Op::PutIfAbsent, 1), Fault::ResponseLost);
        s.put(&key("t"), bytes("torn write")).unwrap_err();
        s.put_if_absent(&key("m"), bytes("manifest")).unwrap_err();
        s.crash().unwrap();
        let state = (
            s.inner().get(&key("t")).is_ok(),
            s.inner().get(&key("m")).is_ok(),
        );
        assert_eq!(
            s.lost().len(),
            [state.0, state.1].iter().filter(|kept| !**kept).count()
        );
        outcomes.insert(state);
    }
    assert_eq!(outcomes.len(), 4, "{outcomes:?}");
}

#[test]
fn a_shared_store_is_reopened_after_the_crash() {
    let store = Arc::new(MemoryStorage::new());
    let s = FaultyStorage::new(Arc::clone(&store), 5);
    s.set_volatile(Some(CrashLoss::Suffix));
    for i in 0..10u8 {
        s.put(&key(&format!("wal/{i}")), Bytes::from(vec![i]))
            .unwrap();
    }
    s.inject(Trigger::Call(11), Fault::Crash);
    s.put(&key("wal/10"), bytes("never")).unwrap_err();
    drop(s);
    let survivors = store.list(&KeyPrefix::new("wal/").unwrap()).unwrap();
    // Suffix loss leaves a prefix of the log: wal/0..k for some k (listed in byte order: 0, 1, ..., 9).
    let names: Vec<String> = survivors
        .iter()
        .map(|m| m.key.as_str().to_owned())
        .collect();
    let expected: Vec<String> = (0..names.len()).map(|i| format!("wal/{i}")).collect();
    assert_eq!(names, expected);
}

/// One write of the model test.
#[derive(Debug, Clone)]
enum Write {
    Put(Key, Bytes),
    PutIfAbsent(Key, Bytes),
    Delete(Key),
    Sync,
}

fn write() -> impl Strategy<Value = Write> {
    let k = prop_oneof![Just("a"), Just("b"), Just("c/d")].prop_map(key);
    let data = prop::collection::vec(any::<u8>(), 0..4).prop_map(Bytes::from);
    prop_oneof![
        4 => (k.clone(), data.clone()).prop_map(|(k, d)| Write::Put(k, d)),
        2 => (k.clone(), data).prop_map(|(k, d)| Write::PutIfAbsent(k, d)),
        2 => k.prop_map(Write::Delete),
        1 => Just(Write::Sync),
    ]
}

type Model = BTreeMap<Key, Bytes>;

/// Applies `writes` to the store, returning per write its call number, key, and the model before and after it.
fn run(s: &FaultyStorage<MemoryStorage>, writes: &[Write]) -> (Vec<(u64, Key)>, Vec<Model>, usize) {
    let mut model = Model::new();
    let mut states = vec![model.clone()];
    let mut calls = Vec::new();
    let mut synced = 0;
    for w in writes {
        let k = match w {
            Write::Put(k, d) => {
                s.put(k, d.clone()).unwrap();
                model.insert(k.clone(), d.clone());
                k
            }
            Write::PutIfAbsent(k, d) => {
                let created = s.put_if_absent(k, d.clone()).unwrap() == PutOutcome::Created;
                assert_eq!(created, model.get(k).is_none_or(|e| e == d));
                model.entry(k.clone()).or_insert_with(|| d.clone());
                k
            }
            Write::Delete(k) => {
                s.delete(k).unwrap();
                model.remove(k);
                k
            }
            Write::Sync => {
                s.sync();
                synced = calls.len();
                continue;
            }
        };
        calls.push((s.calls(), k.clone()));
        states.push(model.clone());
    }
    (calls, states, synced)
}

fn store_state(s: &FaultyStorage<MemoryStorage>) -> Model {
    s.inner()
        .list(&KeyPrefix::new("").unwrap())
        .unwrap()
        .into_iter()
        .map(|m| {
            let data = s.inner().get(&m.key).unwrap();
            (m.key, data)
        })
        .collect()
}

proptest! {
    /// Suffix loss: the store is left exactly as after some prefix of the writes, never before the last sync, and
    /// `lost()` names the writes after that prefix, newest first.
    #[test]
    fn suffix_loss_leaves_the_state_after_a_prefix(
        seed in any::<u64>(),
        writes in prop::collection::vec(write(), 0..30),
    ) {
        let s = faulty(seed);
        s.set_volatile(Some(CrashLoss::Suffix));
        let (calls, states, synced) = run(&s, &writes);
        s.crash().unwrap();
        let lost: Vec<u64> = s.lost().iter().map(|l| l.call).collect();
        let kept = calls.len() - lost.len();
        prop_assert!(kept >= synced, "lost a synced write: kept {} < synced {}", kept, synced);
        let expected_lost: Vec<u64> = calls[kept..].iter().rev().map(|(c, _)| *c).collect();
        prop_assert_eq!(lost, expected_lost);
        prop_assert_eq!(store_state(&s), states[kept].clone(), "seed {}", seed);
    }

    /// Per-key loss: every key is as after some prefix of its own writes (never before the last sync), and the
    /// lost writes of each key are its newest.
    #[test]
    fn per_key_loss_leaves_each_key_after_a_prefix_of_its_writes(
        seed in any::<u64>(),
        writes in prop::collection::vec(write(), 0..30),
    ) {
        let s = faulty(seed);
        s.set_volatile(Some(CrashLoss::PerKey));
        let (calls, states, synced) = run(&s, &writes);
        s.crash().unwrap();
        let lost: Vec<u64> = s.lost().iter().map(|l| l.call).collect();
        let after = store_state(&s);
        for k in ["a", "b", "c/d"].map(key) {
            let own: Vec<usize> = (0..calls.len()).filter(|&i| calls[i].1 == k).collect();
            let kept = own.iter().filter(|&&i| !lost.contains(&calls[i].0)).count();
            // The kept writes are the oldest of the key's own.
            prop_assert!(own[..kept].iter().all(|&i| !lost.contains(&calls[i].0)));
            prop_assert!(own[kept..].iter().all(|&i| lost.contains(&calls[i].0)));
            prop_assert!(own[kept..].iter().all(|&i| i >= synced), "lost a synced write of {}", k);
            // The key's state is the model's right after its last kept write (or before its first write).
            let at = if kept == 0 { own.first().copied().unwrap_or(0) } else { own[kept - 1] + 1 };
            prop_assert_eq!(after.get(&k), states[at].get(&k), "{} seed {}", k, seed);
        }
    }

    /// ROADMAP 2.2.4.2: the same seed and writes lose the same writes.
    #[test]
    fn crash_loss_replays_from_its_seed(
        seed in any::<u64>(),
        writes in prop::collection::vec(write(), 0..20),
        per_key in any::<bool>(),
    ) {
        let lost = || {
            let s = faulty(seed);
            s.set_volatile(Some(if per_key { CrashLoss::PerKey } else { CrashLoss::Suffix }));
            run(&s, &writes);
            s.crash().unwrap();
            (s.lost(), store_state(&s))
        };
        prop_assert_eq!(lost(), lost());
    }
}

#[test]
fn concurrent_writers_and_a_crash_leave_a_consistent_store() {
    // Writes racing a crash either land before it (logged, so undoable) or fail with the crash: the store then
    // only holds values that writer wrote, and every later call fails.
    for seed in 0..20 {
        let s = faulty(seed);
        s.set_volatile(Some(CrashLoss::Suffix));
        std::thread::scope(|scope| {
            for t in 0..4u8 {
                let s = &s;
                scope.spawn(move || {
                    for i in 0..50u8 {
                        if let Err(err) = s.put(&key(&format!("k{t}")), Bytes::from(vec![t, i])) {
                            assert_eq!(err.kind(), ErrorKind::Io, "only the crash fails a write");
                            break;
                        }
                    }
                });
            }
            scope.spawn(|| s.crash().unwrap());
        });
        every_call_fails(&s);
        for t in 0..4u8 {
            if let Ok(v) = s.inner().get(&key(&format!("k{t}"))) {
                assert_eq!((v.len(), v[0]), (2, t), "seed {seed}");
            }
        }
    }
}

/// A clock whose `sleep` signals that it was entered, then blocks until released: it parks a call between the
/// decision on its fault and its write.
struct Gate {
    entered: Mutex<mpsc::Sender<()>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl Clock for Gate {
    fn sleep(&self, _: Duration) {
        self.entered.lock().unwrap().send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
    }
}

#[test]
fn a_write_decided_before_a_crash_does_not_land_after_it() {
    for volatile in [Some(CrashLoss::Suffix), None] {
        write_decided_before_a_crash(volatile);
    }
}

fn write_decided_before_a_crash(volatile: Option<CrashLoss>) {
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let s = faulty(1).with_clock(Gate {
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    });
    s.set_volatile(volatile);
    s.inject(Trigger::Call(1), Fault::Latency(Duration::from_secs(1)));
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| s.put(&key("late"), bytes("after the crash")));
        entered.recv().unwrap();
        s.crash().unwrap();
        release.send(()).unwrap();
        let err = writer.join().unwrap().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Io);
    });
    assert_eq!(
        s.inner().get(&key("late")).unwrap_err().kind(),
        ErrorKind::NotFound,
        "the process is dead: nothing lands after the crash"
    );
}

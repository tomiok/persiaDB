//! The order `ReadOnlyBlobDatabase::refresh` puts two read-only opens of one database in (SPEC §8.4, §8.6): the
//! manifest `seq` first, then, within one `seq`, the replayed WAL records and the WAL end
//! (`ReadOnlyDatabase::wal_end`), which grow together.
//!
//! Invariants:
//! 1. The table of `refresh`'s docs, cell by cell: a higher `seq` is taken and a lower one is `RefreshedOlder`,
//!    whatever the WAL; at the same `seq`, (replayed records, WAL end) both greater or both equal is taken, both less
//!    is `RefreshedOlder`, and each of the six cells where they disagree is `RefreshDiverged`.
//! 2. Within one history (one writer, opens taken one after another), a later open is always taken, and an earlier
//!    one is taken only if it shows the same state, else `RefreshedOlder`: never `RefreshDiverged`.
//! 3. Across two histories forked from one state (a copy put back at the path), the outcome follows the table. A
//!    commit in progress (its segment and Manifest frames written, the slots not yet) moves the container's end but
//!    not the WAL's: it is ordered by its `seq` and WAL like the state before it.
//! 4. A refused refresh changes nothing (`seq`, records, WAL end, live documents) and its error names both opens; a
//!    taken one shows exactly the reopened open. None of these errors is corruption.
//!
//! Every open is of a byte snapshot of the container on its own simulated file, so the opens of one test share no
//! path (`refresh` compares paths, and a container opened on a file has none). Seeds: fixed (`RngSeed::Fixed`),
//! printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)] // Tests: inputs are fixed.

use std::cmp::Ordering::{self, Equal, Greater, Less};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use persia::internal::documents::{Error, ReadOnlyBlobDatabase};
use persia_engine::db::{Database, DatabaseOptions, ManualClock, ReadOnlyDatabase, SegmentIds};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Container, ContainerFile, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

/// Deterministic options: the same steps write the same bytes in both histories.
fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(11),
        ..DatabaseOptions::default()
    }
}

/// One step of a writer.
#[derive(Debug, Clone)]
enum Step {
    /// One put of each id, or one batch of them all (one sync).
    Put {
        ids: Vec<String>,
        batch: bool,
        sync: bool,
    },
    Delete(String),
    /// A commit that flushes nothing.
    CreateCollection,
    /// A commit that flushes every memtable.
    Commit,
    /// A commit cut before its slot write; it ends the history.
    CutCommit,
}

fn put(ids: &[&str]) -> Step {
    Step::Put {
        ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        batch: false,
        sync: true,
    }
}

fn batch(ids: &[&str]) -> Step {
    Step::Put {
        ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        batch: true,
        sync: true,
    }
}

/// The bytes of a created database with the collection `c`.
fn created() -> Vec<u8> {
    let file = SimFile::new();
    let mut db = Database::create_on(file.clone(), [5; 16], options()).unwrap();
    db.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    drop(db);
    file.bytes()
}

/// Opens the database in `bytes` read-write, applies `steps`, and returns the container's bytes before the first
/// step and after each.
fn run(bytes: &[u8], steps: &[Step]) -> Vec<Vec<u8>> {
    let file = SimFile::from_bytes(bytes.to_vec());
    let mut db = Database::open_on(file.clone(), options()).unwrap();
    let mut out = vec![file.bytes()];
    for step in steps {
        match step {
            Step::Put { ids, batch, sync } => {
                let durability = if *sync {
                    Durability::Fsync
                } else {
                    Durability::Async
                };
                let docs: Vec<_> = ids.iter().map(|id| Document::new(id.as_str())).collect();
                if *batch {
                    db.put_batch("c", &docs, durability).unwrap();
                } else {
                    for doc in &docs {
                        db.put("c", doc, durability).unwrap();
                    }
                }
            }
            Step::Delete(id) => db.delete("c", id, Durability::Fsync).unwrap(),
            Step::CreateCollection => {
                let name = format!("k{}", db.manifest().collections().len());
                db.create_collection(SchemaBuilder::new(name).build().unwrap())
                    .unwrap();
            }
            Step::Commit => {
                db.commit().unwrap();
            }
            Step::CutCommit => {
                // The slots the commit wrote are put back as they were: its segment and Manifest frames follow the
                // WAL, and the slots still name the commit before (a commit in progress, as a reader sees it).
                let before = file.bytes();
                db.commit().unwrap();
                let mut cut = file.bytes();
                let slots_end = Container::create_on(SimFile::new(), [0; 16])
                    .unwrap()
                    .superblock()
                    .log_end;
                let slots_end = usize::try_from(slots_end).unwrap();
                cut[..slots_end].copy_from_slice(&before[..slots_end]);
                assert!(
                    std::ptr::eq(step, steps.last().unwrap()),
                    "a cut commit ends a history"
                );
                out.push(cut);
                return out;
            }
        }
        out.push(file.bytes());
    }
    out
}

/// The last state of [`run`].
fn after(bytes: &[u8], steps: &[Step]) -> Vec<u8> {
    run(bytes, steps).pop().unwrap()
}

fn sim() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs
}

/// A read-only open of `bytes`, on a file of its own.
fn open(bytes: &[u8]) -> ReadOnlyDatabase<SimFsFile> {
    let fs = sim();
    let file = fs.create_new(Path::new("/db/app.persia")).unwrap();
    file.write_all_at(bytes, 0).unwrap();
    ReadOnlyDatabase::open_on(file, &options().registry).unwrap()
}

/// A read-only open of `bytes` at `/db/app.persia` on a file system of its own, by path: what a reader lease needs (it
/// reopens the container at its path, on the pack's file system).
fn open_at(bytes: &[u8]) -> (SimFs, ReadOnlyDatabase<SimFsFile>) {
    let fs = sim();
    let file = fs.create_new(Path::new("/db/app.persia")).unwrap();
    file.write_all_at(bytes, 0).unwrap();
    drop(file);
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    let container = Container::open_in(vfs, "/db/app.persia", Mode::ReadOnly).unwrap();
    let db = ReadOnlyDatabase::open_container(container, &options().registry).unwrap();
    (fs, db)
}

/// What a refresh compares: `(seq, replayed WAL records, WAL end)`.
type State = (u64, u64, u64);

fn state<F: ContainerFile + Sync>(db: &ReadOnlyDatabase<F>) -> State {
    (db.seq(), db.replayed(), db.wal_end())
}

/// Every live document of every collection, by collection and id.
fn live<F: ContainerFile + Sync>(db: &ReadOnlyDatabase<F>) -> BTreeSet<(String, String)> {
    let snapshot = db.snapshot();
    let mut all = BTreeSet::new();
    for (name, c) in snapshot.collections() {
        let memtable = c.memtable();
        let mut ids = BTreeSet::new();
        for docid in 0..memtable.doc_count() {
            if memtable.is_live(docid) {
                ids.insert(memtable.document(docid).unwrap().id().to_owned());
            }
        }
        for s in c.segments() {
            for docid in 0..s.doc_count() {
                ids.insert(s.id_of(docid).unwrap());
            }
        }
        for id in ids {
            if c.get(&id).unwrap().is_some() {
                all.insert((name.to_owned(), id));
            }
        }
    }
    all
}

/// What a refresh did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Taken,
    Older,
    Diverged,
}

/// The table of `ReadOnlyBlobDatabase::refresh`'s docs, written out: the oracle the refresh is held to.
fn table(current: State, reopened: State) -> Outcome {
    match reopened.0.cmp(&current.0) {
        Greater => Outcome::Taken,
        Less => Outcome::Older,
        Equal => match (reopened.1.cmp(&current.1), reopened.2.cmp(&current.2)) {
            (Greater, Greater) | (Equal, Equal) => Outcome::Taken,
            (Less, Less) => Outcome::Older,
            _ => Outcome::Diverged,
        },
    }
}

/// Refreshes a reader of `current` with an open of `reopened` and checks invariant 4: a taken open is what the reader
/// shows after, and a refused one leaves the reader as it was, with an error naming both opens.
fn refresh(current: &[u8], reopened: &[u8]) -> Outcome {
    let (fs, current) = open_at(current);
    let mut reader =
        ReadOnlyBlobDatabase::with_pack_in(current, Arc::new(fs), "/db/app.persia.blobs").unwrap();
    let reopened = open_at(reopened).1;
    let (before, before_live) = (state(reader.database()), live(reader.database()));
    let (wanted, wanted_live) = (state(&reopened), live(&reopened));
    let refreshed = reader.refresh(reopened);
    let outcome = match &refreshed {
        Ok(found) => {
            assert!(!found, "no pack: nothing new in it");
            assert_eq!(state(reader.database()), wanted);
            assert_eq!(live(reader.database()), wanted_live);
            return Outcome::Taken;
        }
        Err(Error::RefreshedOlder {
            current,
            reopened,
            current_replayed,
            reopened_replayed,
            current_end,
            reopened_end,
        }) => {
            assert_eq!((*current, *current_replayed, *current_end), before);
            assert_eq!((*reopened, *reopened_replayed, *reopened_end), wanted);
            Outcome::Older
        }
        Err(Error::RefreshDiverged {
            current,
            reopened,
            current_replayed,
            reopened_replayed,
            current_end,
            reopened_end,
        }) => {
            assert_eq!((*current, *current_replayed, *current_end), before);
            assert_eq!((*reopened, *reopened_replayed, *reopened_end), wanted);
            Outcome::Diverged
        }
        Err(e) => panic!("{e:?}"),
    };
    assert!(!refreshed.unwrap_err().is_corruption());
    assert_eq!(state(reader.database()), before);
    assert_eq!(live(reader.database()), before_live);
    outcome
}

/// An id whose one put ends the WAL where a batch of `x` and `y` does, each logged on `base`: the same WAL end with
/// one record fewer.
fn id_as_long_as_a_batch(base: &[u8]) -> String {
    let wal_end = |step: Step| open(&after(base, &[step])).wal_end();
    let target = wal_end(batch(&["x", "y"]));
    (1..=512)
        .map(|n| "i".repeat(n))
        .find(|id| wal_end(put(&[id.as_str()])) == target)
        .unwrap_or_else(|| panic!("no single put ends the WAL at {target}"))
}

/// A cell of the table: its name, the reader's history, the reopened open's, the order of the reopened open's
/// `(seq, replayed, WAL end)` against the reader's, and the outcome.
type Cell = (
    &'static str,
    Vec<Step>,
    Vec<Step>,
    (Ordering, Ordering, Ordering),
    Outcome,
);

/// Invariant 1: every cell of the table, each from two histories forked from one created database: the reader's
/// (`a`) and the reopened open's (`b`).
#[test]
fn refresh_follows_the_table_in_every_cell() {
    let base = created();
    let long = "l".repeat(500);
    let same_end = id_as_long_as_a_batch(&base);
    // Many records, whose WAL ends past a commit's segment and Manifest frames.
    let many: Vec<String> = (0..40).map(|i| format!("{i:0>100}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let commit = Step::Commit;
    let cells: Vec<Cell> = vec![
        // A higher `seq`, whatever the WAL.
        (
            "higher, both fewer",
            vec![put(&many)],
            vec![put(&["x"]), commit.clone()],
            (Greater, Less, Less),
            Outcome::Taken,
        ),
        (
            "higher, fewer, later",
            vec![put(&["x", "y"])],
            vec![put(&["z"]), commit.clone()],
            (Greater, Less, Greater),
            Outcome::Taken,
        ),
        (
            "higher, both more",
            vec![],
            vec![put(&["x"]), commit.clone(), put(&["y", "z"])],
            (Greater, Greater, Greater),
            Outcome::Taken,
        ),
        // A lower `seq`, whatever the WAL.
        (
            "lower, both more",
            vec![put(&["x"]), commit.clone()],
            vec![put(&many)],
            (Less, Greater, Greater),
            Outcome::Older,
        ),
        (
            "lower, more, earlier",
            vec![put(&["x"]), commit.clone(), put(&[&long])],
            vec![put(&["y", "z"])],
            (Less, Greater, Less),
            Outcome::Older,
        ),
        (
            "lower, both fewer",
            vec![put(&["x"]), commit.clone(), put(&["y", "z"])],
            vec![put(&["w"])],
            (Less, Less, Less),
            Outcome::Older,
        ),
        // The same `seq`: the nine cells of (replayed, WAL end).
        (
            "both more",
            vec![put(&["a"])],
            vec![put(&["b", "c"])],
            (Equal, Greater, Greater),
            Outcome::Taken,
        ),
        (
            "more, same end",
            vec![put(&[&same_end])],
            vec![batch(&["x", "y"])],
            (Equal, Greater, Equal),
            Outcome::Diverged,
        ),
        (
            "more, earlier",
            vec![put(&[&long])],
            vec![put(&["b", "c"])],
            (Equal, Greater, Less),
            Outcome::Diverged,
        ),
        (
            "same count, later",
            vec![put(&["a"])],
            vec![put(&["a-longer-id"])],
            (Equal, Equal, Greater),
            Outcome::Diverged,
        ),
        (
            "the same",
            vec![put(&["a"])],
            vec![put(&["b"])],
            (Equal, Equal, Equal),
            Outcome::Taken,
        ),
        (
            "same count, earlier",
            vec![put(&["a-longer-id"])],
            vec![put(&["a"])],
            (Equal, Equal, Less),
            Outcome::Diverged,
        ),
        (
            "fewer, later",
            vec![put(&["b", "c"])],
            vec![put(&[&long])],
            (Equal, Less, Greater),
            Outcome::Diverged,
        ),
        (
            "fewer, same end",
            vec![batch(&["x", "y"])],
            vec![put(&[&same_end])],
            (Equal, Less, Equal),
            Outcome::Diverged,
        ),
        (
            "both fewer",
            vec![put(&["b", "c"])],
            vec![put(&["a"])],
            (Equal, Less, Less),
            Outcome::Older,
        ),
        // A commit in progress moves the container's end, not the WAL's: still the same `seq` and WAL.
        (
            "the same, a commit in progress",
            vec![put(&["a"])],
            vec![put(&["a"]), Step::CutCommit],
            (Equal, Equal, Equal),
            Outcome::Taken,
        ),
        (
            "both more, a commit in progress",
            vec![put(&["a"])],
            vec![put(&["a", "b"]), Step::CutCommit],
            (Equal, Greater, Greater),
            Outcome::Taken,
        ),
        (
            "both fewer, a commit in progress",
            vec![put(&["a", "b"])],
            vec![put(&["a"]), Step::CutCommit],
            (Equal, Less, Less),
            Outcome::Older,
        ),
    ];
    let mut seen = BTreeSet::new();
    for (name, a, b, order, outcome) in cells {
        let (current, reopened) = (after(&base, &a), after(&base, &b));
        let (c, r) = (state(&open(&current)), state(&open(&reopened)));
        assert_eq!(
            (r.0.cmp(&c.0), r.1.cmp(&c.1), r.2.cmp(&c.2)),
            order,
            "{name}: the histories do not make this cell ({c:?} -> {r:?})"
        );
        if matches!(b.last(), Some(Step::CutCommit)) {
            let ends = (
                open(&current).container().end(),
                open(&reopened).container().end(),
            );
            assert!(
                ends.1 > ends.0,
                "{name}: the commit's frames follow: {ends:?}"
            );
        }
        assert_eq!(table(c, r), outcome, "{name}: the oracle");
        assert_eq!(refresh(&current, &reopened), outcome, "{name}");
        if order.0 == Equal {
            seen.insert((order.1, order.2));
        }
    }
    assert_eq!(seen.len(), 9, "every cell at the same seq: {seen:?}");
}

fn step() -> impl Strategy<Value = Step> {
    let id = (0u8..6, 0usize..60).prop_map(|(n, pad)| format!("d{n}{}", "p".repeat(pad)));
    prop_oneof![
        6 => (prop::collection::vec(id.clone(), 1..4), any::<bool>(), any::<bool>())
            .prop_map(|(ids, batch, sync)| Step::Put { ids, batch, sync }),
        2 => id.prop_map(Step::Delete),
        1 => Just(Step::CreateCollection),
        1 => Just(Step::Commit),
    ]
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if cfg!(feature = "slow") { 256 } else { 24 })
}

proptest! {
    #![proptest_config(Config {
        cases: cases(),
        rng_seed: RngSeed::Fixed(0x9_2_1_5),
        failure_persistence: None,
        ..Config::default()
    })]

    /// Invariants 2 to 4 over random histories: every pair of opens of history `a`, then the last open of `a`
    /// against every open of `b`, forked from the same state, and back.
    #[test]
    fn refresh_never_refuses_a_later_open_and_follows_the_table_across_histories(
        shared in prop::collection::vec(step(), 0..4),
        a in prop::collection::vec(step(), 0..7),
        mut b in prop::collection::vec(step(), 0..7),
        cut in any::<bool>(),
    ) {
        if cut {
            b.push(Step::CutCommit);
        }
        let fork = after(&created(), &shared);
        let opens = run(&fork, &a);
        let states: Vec<State> = opens.iter().map(|bytes| state(&open(bytes))).collect();
        for (i, current) in opens.iter().enumerate() {
            for (j, reopened) in opens.iter().enumerate() {
                let wanted = if j >= i || states[j] == states[i] {
                    Outcome::Taken
                } else {
                    Outcome::Older
                };
                prop_assert_eq!(table(states[i], states[j]), wanted, "open {} then {}", i, j);
                prop_assert_eq!(refresh(current, reopened), wanted, "open {} then {}", i, j);
            }
        }
        let last = opens.last().unwrap();
        let others = run(&fork, &b);
        if cut {
            let n = others.len();
            prop_assert_eq!(state(&open(&others[n - 1])), state(&open(&others[n - 2])), "a commit in progress");
        }
        for (j, other) in others.iter().enumerate() {
            let other_state = state(&open(other));
            let (&a_state, a_bytes) = (states.last().unwrap(), last);
            prop_assert_eq!(refresh(a_bytes, other), table(a_state, other_state), "a then b {}", j);
            prop_assert_eq!(refresh(other, a_bytes), table(other_state, a_state), "b {} then a", j);
        }
    }
}

/// A reader lease reopens the documents' container at its path to read its committed seq: documents of a container
/// opened without a path are refused (`NoContainerPath`), and nothing is created.
#[test]
fn documents_without_a_container_path_cannot_take_a_reader_lease() {
    let fs = sim();
    let e = ReadOnlyBlobDatabase::with_pack_in(
        open(&created()),
        Arc::new(fs.clone()),
        "/db/app.persia.blobs",
    )
    .unwrap_err();
    assert!(matches!(e, Error::NoContainerPath), "{e:?}");
    assert_eq!(fs.read_dir(Path::new("/db")).unwrap(), Vec::new());
}

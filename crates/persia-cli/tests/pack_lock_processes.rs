//! The blob pack's writer lock and side files across real processes (SPEC §9.3, §4.5.1; ROADMAP 9.2.15, 9.2.10). The
//! children are this test binary re-executed (`blob_support`); they report each step on stdout and wait on stdin, so
//! no test sleeps.
//!
//! Invariants:
//! 1. **One writer, even with no pack file** (9.2.15): processes released at once to open the same database
//!    read-write, before any pack exists, end with exactly one writer; every other fails on the container's lock or
//!    on the pack's (`PackLocked`), never opening. A winner that drops its database but keeps the pack store keeps the
//!    pack's lock with no pack file: a later process opens the container and fails on the pack, sweeping nothing and
//!    creating no pack. Seeded rounds vary the number of racers, the order they are released in and whether the winner
//!    keeps its database or only its pack store.
//! 2. **A stopped or killed writer** (9.2.15): a writer process stopped (`SIGSTOP`) mid-run keeps both locks; once it
//!    is killed (`SIGKILL`), the locks are gone with it, the next process opens read-write and recovers, every
//!    acknowledged (`Fsync`) document reads its blob back, nothing it shows was never started, and `persia verify
//!    --deep` passes.
//! 3. **`persia container sweep` beside a pack writer** (9.2.10): while a process holds the database, the sweep fails
//!    on the container's lock and removes nothing. While a process holds only the pack store (the container is free),
//!    `persia container files` reports the pack's lock and temporaries in use and the sweep keeps them (it removes the
//!    container's own stale temporary). Once that process is killed, they are free and the sweep removes the pack's
//!    stale temporaries; the lock file and the lease directory (and the lease in it) are never removed, and the pack
//!    still verifies and reads.
//!
//! Seeds: `PERSIA_TEST_SEED` (printed) sets the race schedules and the kill point; `PERSIA_TEST_ROUNDS` overrides the
//! number of race rounds. Blob contents are a function of the document id. On a failure the temp dir is kept and its
//! path printed.
#![cfg(unix)] // `flock` locks, `SIGSTOP`/`SIGKILL`.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::too_many_lines
)] // Tests: inputs are generated or fixed.

mod blob_support;

use std::collections::BTreeSet;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use blob_support::{
    COLLECTION, ChildIo, KeptOnFailure, Rng, base_seed, blob_of_id, child_env, content, env_count,
    live_ids, persia, report, row, serial, verify_ok, wait_for_eof,
};
use persia::internal::documents::{Blob, BlobDatabase, Error, ReadOnlyBlobDatabase};
use persia_engine::db::{DatabaseError, DatabaseOptions};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::container::Error as ContainerError;

/// The share of the blobs that span at least one 4 MiB chunk.
const BIG_PERCENT: u64 = 10;

#[cfg(not(feature = "slow"))]
const ROUNDS: u64 = 12;
#[cfg(feature = "slow")]
const ROUNDS: u64 = 64;

/// A stale temporary name of a pack creation or rewrite, as the pack names them (lowercase hex).
const PACK_TMP: &str = "app.persia.blobs.tmp.0123456789abcdef.7";
const PACK_TMP_2: &str = "app.persia.blobs.tmp.fedcba9876543210.12";
/// A stale temporary of a container create: no lock held, so always removable by a sweep that holds the container.
const CONTAINER_TMP: &str = "app.persia.tmp.0123456789abcdef.0";

// ---------------------------------------------------------------------------------------------------------------
// Child roles
// ---------------------------------------------------------------------------------------------------------------

/// The entry point of every child process; a no-op in a normal test run.
#[test]
fn child_role() {
    let Some(env) = child_env() else {
        return;
    };
    match env.role.as_str() {
        "racer-db" => racer(&env.path, &env.prefix, false),
        "racer-store" => racer(&env.path, &env.prefix, true),
        "try-open" => try_open(&env.path),
        "store-holder" => store_holder(&env.path, &env.prefix),
        "holder" => holder(&env.path, &env.prefix),
        "write-hold" => write_hold(&env.path, &env.prefix, env.count),
        other => panic!("unknown child role {other}"),
    }
}

/// The blob of document `id`, as every role derives it.
fn bytes_of(id: &str) -> Vec<u8> {
    let (size, cseed) = blob_of_id(id, BIG_PERCENT);
    content(size, cseed)
}

/// Puts document `$id` with its blob (`bytes_of`) at `Fsync` (a macro: the pack store type is `persia-blob`'s, which
/// this crate does not depend on).
macro_rules! put {
    ($db:expr, $id:expr) => {
        $db.put(
            COLLECTION,
            &Document::new($id),
            Blob::from_reader(&bytes_of($id)[..]),
            Durability::Fsync,
        )
        .unwrap()
    };
}

/// What a read-write open returned: `opened`, `locked` (the container's lock), `pack-locked` (the pack's) or
/// `error <e>`.
fn outcome<T>(opened: &Result<T, Error>) -> String {
    match opened {
        Ok(_) => "opened".to_owned(),
        Err(Error::Database(e))
            if matches!(**e, DatabaseError::Container(ContainerError::Locked)) =>
        {
            "locked".to_owned()
        }
        // persia-blob's `PackLocked`, named by its message: this crate does not depend on persia-blob.
        Err(e @ Error::Blob(_)) if e.to_string().contains("locked by another writer") => {
            "pack-locked".to_owned()
        }
        Err(e) => format!("error {e}"),
    }
}

/// A read-write open of the database at `path`, reported ([`outcome`]) and dropped.
fn try_open(path: &Path) {
    report(&outcome(&BlobDatabase::open(
        path,
        DatabaseOptions::default(),
    )));
}

/// Reports `ready`, waits for a `go` line, opens read-write and reports the outcome ([`outcome`]). A winner keeps the
/// database (or, with `store_only`, drops it and keeps only its pack store) until stdin closes; with the database, a
/// `put` line puts document `<prefix>` and reports `ack`.
fn racer(path: &Path, prefix: &str, store_only: bool) {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    report("ready");
    assert_eq!(lines.next().unwrap().unwrap(), "go");
    let opened = BlobDatabase::open(path, DatabaseOptions::default());
    report(&outcome(&opened));
    let Ok(mut db) = opened else {
        return;
    };
    if store_only {
        let kept = Arc::clone(db.blobs().store());
        drop(db);
        if let Some(line) = lines.next() {
            panic!("a store-only racer got {}", line.unwrap());
        }
        drop(kept);
        return;
    }
    for line in lines {
        assert_eq!(line.unwrap(), "put");
        put!(db, prefix);
        report("ack");
    }
}

/// Opens read-write and drops the database, keeping only its pack store, with no `put` (so no pack file), until
/// stdin closes. With a prefix, first puts document `<prefix>` (the pack exists then).
fn store_holder(path: &Path, prefix: &str) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    if !prefix.is_empty() {
        put!(db, prefix);
    }
    let kept = Arc::clone(db.blobs().store());
    drop(db);
    report("ready");
    wait_for_eof();
    drop(kept);
}

/// Holds the database read-write with document `<prefix>` until stdin closes.
fn holder(path: &Path, prefix: &str) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    put!(db, prefix);
    report("ready");
    wait_for_eof();
}

/// Puts `<prefix>-0` .. `<prefix>-<count - 1>` at `Fsync`, committing every 4, reporting `start <id>` and
/// `ack <id>`, then `holding`, and holds the database until stdin closes.
fn write_hold(path: &Path, prefix: &str, count: u64) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    for n in 0..count {
        let id = format!("{prefix}-{n}");
        report(&format!("start {id}"));
        put!(db, &id);
        report(&format!("ack {id}"));
        if n % 4 == 3 {
            db.commit().unwrap();
        }
    }
    report("holding");
    wait_for_eof();
}

// ---------------------------------------------------------------------------------------------------------------
// Parent helpers
// ---------------------------------------------------------------------------------------------------------------

/// A database at `<dir>/app.persia` with the collection, committed, and no pack.
fn create(dir: &Path) -> PathBuf {
    let path = dir.join("app.persia");
    let mut db = BlobDatabase::create(&path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new(COLLECTION).build().unwrap())
        .unwrap();
    db.commit().unwrap();
    path
}

fn pack_of(path: &Path) -> PathBuf {
    path.with_file_name("app.persia.blobs")
}

fn lock_of(path: &Path) -> PathBuf {
    path.with_file_name("app.persia.blobs.lock")
}

/// The names in `dir` that start with `app.persia.blobs.tmp.`.
fn pack_temporaries(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("app.persia.blobs.tmp."))
        .collect()
}

/// Every live document of the database at `path`, read-only, with its blob checked byte for byte; returns the ids.
fn check_blobs(path: &Path, ctx: &str) -> BTreeSet<String> {
    let ro = ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default())
        .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"));
    let snapshot = ro.database().snapshot();
    let c = snapshot.collection(COLLECTION).unwrap();
    let ids = live_ids(&snapshot, COLLECTION);
    for id in &ids {
        let doc = c.get(id).unwrap().unwrap();
        let blob = doc
            .blob_ref()
            .unwrap_or_else(|| panic!("{ctx}: {id} has no blob"));
        let mut out = Vec::new();
        ro.get_blob(blob)
            .unwrap_or_else(|e| panic!("{ctx}: get_blob({id}): {e}"))
            .read_to_end(&mut out)
            .unwrap();
        assert!(out == bytes_of(id), "{ctx}: {id}'s blob differs");
    }
    ids
}

/// One line of `persia container files|sweep`: `(action, kind, state, file name)`.
type SideRow = (String, String, String, String);

/// `persia container <command> <path>`, which must succeed: its rows, with each path cut to its file name.
fn container_rows(command: &str, path: &Path, ctx: &str) -> Vec<SideRow> {
    let out = persia(&["container", command, path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{ctx}: persia container {command} exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            assert_eq!(f.len(), 4, "{ctx}: {l:?}");
            let name = Path::new(f[3]).file_name().unwrap().to_str().unwrap();
            (f[0].into(), f[1].into(), f[2].into(), name.into())
        })
        .collect()
}

fn side(action: &str, kind: &str, state: &str, name: &str) -> SideRow {
    (action.into(), kind.into(), state.into(), name.into())
}

/// Sends `SIGSTOP` or `SIGCONT` to `pid` with the `kill` utility (this crate has no `libc`).
fn signal(pid: u32, sig: &str) {
    let status = std::process::Command::new("kill")
        .args([sig, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {sig} {pid}: {status}");
}

// ---------------------------------------------------------------------------------------------------------------
// 1. One writer, even with no pack file
// ---------------------------------------------------------------------------------------------------------------

/// Seeded rounds of 2 or 3 processes released at once on a database with no pack: exactly one opens read-write.
#[test]
fn racing_writer_processes_with_no_pack_file_end_with_exactly_one_writer() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x5eed_9215_0ace);
    let rounds = env_count("PERSIA_TEST_ROUNDS", ROUNDS);
    let mut rng = Rng(seed);
    let started = Instant::now();
    let (mut locked, mut pack_locked, mut store_winners) = (0_u32, 0_u32, 0_u32);
    for round in 0..rounds {
        let dir = KeptOnFailure::new();
        let path = create(dir.path());
        assert!(!pack_of(&path).exists());
        let n = usize::try_from(rng.range(2, 4)).unwrap();
        let mut racers: Vec<(ChildIo, bool, String)> = (0..n)
            .map(|i| {
                let store_only = rng.chance(50);
                let role = if store_only {
                    "racer-store"
                } else {
                    "racer-db"
                };
                let id = format!("r{round}-{i}");
                (ChildIo::spawn(role, &path, 0, &id, 0), store_only, id)
            })
            .collect();
        let ctx = format!("round {round}");
        for (r, _, _) in &racers {
            assert_eq!(r.expect(&ctx), "ready", "{ctx}");
        }
        // Released back to back, in a seeded order.
        let mut order: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            order.swap(
                i,
                usize::try_from(rng.range(0, u64::try_from(i + 1).unwrap())).unwrap(),
            );
        }
        for &i in &order {
            racers[i].0.send("go");
        }
        let outcomes: Vec<String> = racers.iter().map(|(r, _, _)| r.expect(&ctx)).collect();
        let winners: Vec<usize> = (0..n).filter(|&i| outcomes[i] == "opened").collect();
        assert_eq!(
            winners.len(),
            1,
            "{ctx}: release order {order:?}, outcomes {outcomes:?}"
        );
        for (i, o) in outcomes.iter().enumerate() {
            match o.as_str() {
                "opened" => {}
                "locked" => locked += 1,
                "pack-locked" => pack_locked += 1,
                other => panic!("{ctx}: racer {i}: {other}"),
            }
        }
        // Nobody wrote yet: a locked-out open created no pack and left no temporary.
        assert!(!pack_of(&path).exists(), "{ctx}: a pack appeared");
        assert!(pack_temporaries(dir.path()).is_empty(), "{ctx}");
        assert!(lock_of(&path).exists(), "{ctx}: no lock file");
        // A late process is locked out too, on the pack's lock if the winner kept only its store.
        let late = ChildIo::spawn("try-open", &path, 0, "", 0);
        let w = winners[0];
        let late_outcome = late.expect(&ctx);
        late.finish();
        let expected = if racers[w].1 { "pack-locked" } else { "locked" };
        assert_eq!(late_outcome, expected, "{ctx}: late open");
        assert!(!pack_of(&path).exists(), "{ctx}: the late open made a pack");

        let mut expected_ids = BTreeSet::from(["after".to_owned()]);
        let mut winner = None;
        for (i, (r, store_only, id)) in racers.into_iter().enumerate() {
            if i == w {
                winner = Some((r, store_only, id));
            } else {
                assert_eq!(r.finish(), Vec::<String>::new(), "{ctx}");
            }
        }
        let (mut r, store_only, id) = winner.unwrap();
        if store_only {
            store_winners += 1;
        } else {
            r.send("put");
            assert_eq!(r.expect(&ctx), "ack");
            assert!(
                pack_of(&path).exists(),
                "{ctx}: the winner's put made no pack"
            );
            expected_ids.insert(id);
        }
        r.finish();
        // The locks went with the winner: the next writer opens and writes.
        let mut db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
        put!(db, "after");
        drop(db);
        assert_eq!(check_blobs(&path, &ctx), expected_ids, "{ctx}");
        assert!(pack_temporaries(dir.path()).is_empty(), "{ctx}");
    }
    eprintln!(
        "{rounds} rounds in {:.1?}: losers locked on the container {locked}, on the pack {pack_locked}; \
         {store_winners} winners kept only the pack store",
        started.elapsed()
    );
}

/// The 9.2.15 case itself: a process that holds the pack store before any pack file exists locks a second writer
/// process out of the pack. The loser sweeps nothing and makes no pack; once the holder is killed, the next open
/// succeeds and sweeps.
#[test]
fn a_pack_store_held_before_the_pack_exists_locks_out_another_process_until_killed() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    let path = create(dir.path());

    let holder = ChildIo::spawn("store-holder", &path, 0, "", 0);
    assert_eq!(holder.expect("holder"), "ready");
    assert!(!pack_of(&path).exists(), "no put, so no pack file");
    assert!(lock_of(&path).exists());
    let stale = dir.path().join(PACK_TMP);
    std::fs::write(&stale, b"stale").unwrap();
    assert_eq!(
        container_rows("files", &path, "files beside the holder"),
        [
            side("found", "lock", "in-use", "app.persia.blobs.lock"),
            side("found", "temporary", "in-use", PACK_TMP),
        ]
    );
    for attempt in 0..3 {
        let ctx = format!("second writer {attempt}");
        let second = ChildIo::spawn("try-open", &path, 0, "", 0);
        assert_eq!(second.expect(&ctx), "pack-locked", "{ctx}");
        second.finish();
        assert!(stale.exists(), "{ctx}: the locked-out open swept");
        assert!(
            !pack_of(&path).exists(),
            "{ctx}: the locked-out open made a pack"
        );
    }
    assert_eq!(holder.kill(), Vec::<String>::new());
    let next = ChildIo::spawn("try-open", &path, 0, "", 0);
    assert_eq!(next.expect("after the kill"), "opened");
    next.finish();
    assert!(
        !stale.exists(),
        "the open after the kill kept the stale temporary"
    );
    assert!(lock_of(&path).exists(), "the lock file is never removed");
}

// ---------------------------------------------------------------------------------------------------------------
// 2. A stopped or killed writer
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_stopped_then_killed_writer_process_releases_its_locks_and_the_next_writer_recovers() {
    const COUNT: u64 = 24;
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x5eed_9215_c111);
    let mut rng = Rng(seed);
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    // Killed right after a seeded `ack` (inside the next put), or once it holds the database idle.
    let kill_after = rng.range(1, COUNT + 1);
    eprintln!("stop and kill after ack {kill_after} of {COUNT}");

    let started = Instant::now();
    let writer = ChildIo::spawn("write-hold", &path, 0, "w", COUNT);
    let (mut acked, mut begun) = (BTreeSet::new(), BTreeSet::new());
    loop {
        let msg = writer.expect("writer");
        if let Some(id) = msg.strip_prefix("start ") {
            begun.insert(id.to_owned());
        } else if let Some(id) = msg.strip_prefix("ack ") {
            acked.insert(id.to_owned());
            if acked.len() == usize::try_from(kill_after).unwrap() {
                break;
            }
        } else {
            panic!("writer: {msg}");
        }
    }
    signal(writer.pid(), "-STOP");
    for attempt in 0..2 {
        let ctx = format!("beside the stopped writer {attempt}");
        let second = ChildIo::spawn("try-open", &path, 0, "", 0);
        assert_eq!(second.expect(&ctx), "locked", "{ctx}");
        second.finish();
    }
    // Reports made before the stop count as begun (an `ack` among them: acknowledged).
    for msg in writer.kill() {
        if let Some(id) = msg.strip_prefix("start ") {
            begun.insert(id.to_owned());
        } else if let Some(id) = msg.strip_prefix("ack ") {
            acked.insert(id.to_owned());
        }
    }
    let killed = Instant::now();
    let next = ChildIo::spawn("try-open", &path, 0, "", 0);
    assert_eq!(next.expect("after the kill"), "opened");
    next.finish();
    let reopened = killed.elapsed();
    verify_ok(&["--deep"], &path, "after the kill");
    let ids = check_blobs(&path, "after the kill");
    for id in &acked {
        assert!(ids.contains(id), "lost acknowledged {id}");
    }
    for id in &ids {
        assert!(begun.contains(id), "{id} was never started");
    }
    assert!(pack_temporaries(dir.path()).is_empty());
    // And it writes on.
    let mut db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
    put!(db, "after");
    drop(db);
    verify_ok(&["--deep"], &path, "after the next writer");
    let mut with_after = ids.clone();
    with_after.insert("after".to_owned());
    assert_eq!(check_blobs(&path, "after the next writer"), with_after);
    eprintln!(
        "{} acknowledged, {} shown after the kill; next writer open+recovery {reopened:.1?}; total {:.1?}",
        acked.len(),
        ids.len(),
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 3. `persia container sweep` beside a pack writer
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn container_sweep_keeps_the_pack_temporaries_of_a_live_writer_process_and_removes_them_after_it_dies()
 {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    let plant = |names: &[&str]| {
        for name in names {
            std::fs::write(dir.path().join(name), b"a crashed write").unwrap();
        }
    };
    let leases = dir.path().join("app.persia.blobs.leases");
    let lease = leases.join("0123456789abcdef0123456789abcdef.lease");
    std::fs::create_dir(&leases).unwrap();
    std::fs::write(&lease, b"").unwrap();

    // A process holds the database: the sweep fails on the container's lock and removes nothing.
    let holder = ChildIo::spawn("holder", &path, 0, "held", 0);
    assert_eq!(holder.expect("holder"), "ready");
    plant(&[PACK_TMP, PACK_TMP_2, CONTAINER_TMP]);
    let out = persia(&["container", "sweep", path.to_str().unwrap()]);
    assert!(!out.status.success(), "the sweep beside a writer succeeded");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("open by a writer"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for name in [PACK_TMP, PACK_TMP_2, CONTAINER_TMP] {
        assert!(
            dir.path().join(name).exists(),
            "{name} removed beside a writer"
        );
    }
    holder.finish();

    // A process holds only the pack store (it opened read-write, which swept the planted pack temporaries).
    let holder = ChildIo::spawn("store-holder", &path, 0, "held", 0);
    assert_eq!(holder.expect("store holder"), "ready");
    assert_eq!(
        pack_temporaries(dir.path()),
        BTreeSet::new(),
        "the writer's open kept stale pack temporaries"
    );
    let pack_before = std::fs::read(pack_of(&path)).unwrap();
    plant(&[PACK_TMP, PACK_TMP_2]);
    let in_use = [
        side("found", "leases", "unknown", "app.persia.blobs.leases"),
        side("found", "lock", "in-use", "app.persia.blobs.lock"),
        side("found", "temporary", "in-use", PACK_TMP),
        side("found", "temporary", "in-use", PACK_TMP_2),
        side("found", "temporary", "free", CONTAINER_TMP),
    ];
    assert_eq!(
        container_rows("files", &path, "files beside the store"),
        in_use
    );
    for attempt in 0..2 {
        let ctx = format!("sweep beside the store {attempt}");
        let rows = container_rows("sweep", &path, &ctx);
        // `removed` rows first, then `kept`, each sorted by path (as the CLI prints them).
        let mut expected = Vec::new();
        if attempt == 0 {
            expected.push(side("removed", "temporary", "free", CONTAINER_TMP));
        }
        expected.extend([
            side("kept", "leases", "unknown", "app.persia.blobs.leases"),
            side("kept", "lock", "in-use", "app.persia.blobs.lock"),
            side("kept", "temporary", "in-use", PACK_TMP),
            side("kept", "temporary", "in-use", PACK_TMP_2),
        ]);
        assert_eq!(rows, expected, "{ctx}");
        assert_eq!(
            pack_temporaries(dir.path()),
            BTreeSet::from([PACK_TMP.to_owned(), PACK_TMP_2.to_owned()]),
            "{ctx}"
        );
        assert!(!dir.path().join(CONTAINER_TMP).exists(), "{ctx}");
        assert!(
            std::fs::read(pack_of(&path)).unwrap() == pack_before,
            "{ctx}"
        );
    }

    // Killed: the pack's lock and temporaries are free, and the sweep removes the temporaries only.
    assert_eq!(holder.kill(), Vec::<String>::new());
    assert_eq!(
        container_rows("files", &path, "files after the kill"),
        [
            side("found", "leases", "unknown", "app.persia.blobs.leases"),
            side("found", "lock", "free", "app.persia.blobs.lock"),
            side("found", "temporary", "free", PACK_TMP),
            side("found", "temporary", "free", PACK_TMP_2),
        ]
    );
    assert_eq!(
        container_rows("sweep", &path, "sweep after the kill"),
        [
            side("removed", "temporary", "free", PACK_TMP),
            side("removed", "temporary", "free", PACK_TMP_2),
            side("kept", "leases", "unknown", "app.persia.blobs.leases"),
            side("kept", "lock", "free", "app.persia.blobs.lock"),
        ]
    );
    assert!(pack_temporaries(dir.path()).is_empty());
    assert!(lock_of(&path).exists(), "the sweep removed the lock file");
    assert!(lease.exists(), "the sweep touched the lease directory");
    // Nothing left to do: a second sweep removes nothing and keeps the lock and the leases.
    assert_eq!(
        container_rows("sweep", &path, "second sweep"),
        [
            side("kept", "leases", "unknown", "app.persia.blobs.leases"),
            side("kept", "lock", "free", "app.persia.blobs.lock"),
        ]
    );
    let rows = verify_ok(&["--deep"], &path, "after the sweeps");
    assert_eq!(row(&rows, "pack"), "1 blobs bound");
    assert_eq!(
        check_blobs(&path, "after the sweeps"),
        BTreeSet::from(["held".to_owned()])
    );
    // The lock file the sweep kept is the one the next writer takes: it opens, and locks a second one out.
    let mut db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
    put!(db, "after");
    let second = ChildIo::spawn("try-open", &path, 0, "", 0);
    assert_eq!(second.expect("beside the last writer"), "locked");
    second.finish();
    drop(db);
}

//! Real-file, real-process crash test of the commit path (ROADMAP 6.3; SPEC §4.5, §8.2.1, §8.6).
//!
//! A writer child (this test binary re-executed with [`ROLE_ENV`] set, as in `persia-storage`'s
//! `container_process.rs`) opens a database in a temp-dir container file, `put`s documents with
//! [`Durability::Fsync`] and reports each id on stdout once `put` returned, and commits every few puts. The parent
//! SIGKILLs it after a seeded number of acknowledgements and a seeded delay, reopens the database with
//! [`Database::open`], and checks it; then the next child continues on the same file (crash after crash).
//!
//! Invariants, after every kill:
//! 1. The reopen succeeds, and every id any child acknowledged so far is present with its body: in the memtable
//!    (replayed WAL), else in the newest segment that has it (SPEC §8.3, §8.6).
//! 2. No phantom: every document present is one a child acknowledged, or the single put a killed child had in
//!    flight (its next id), with the body that id's put wrote.
//! 3. The committed `seq` is at least the last `seq` a child reported from `commit()`.
//! 4. Recovery is durable: `commit()` after the reopen, then another reopen, shows the same documents.
//!
//! SIGKILL kills the process, not the machine: writes the child never synced stay in the page cache. So this tests
//! process-crash recovery (a put or a commit cut at any step, repeated recoveries of one file, the lock released by
//! the kernel); power loss is simulated on a `SimFile` by `database_crash.rs`.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed; a failure is replayed with
//! `PERSIA_TEST_SEED=<seed> cargo test -p persia-engine --test database_process`. The kill points also depend on
//! timing, so a replay reproduces the schedule (counts, delays, commit cadence), not the exact instant.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use persia_engine::db::{
    Database, DatabaseError, DatabaseOptions, ManualClock, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::IdState;
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::Error as ContainerError;

/// Selects the child role of a re-executed test binary.
const ROLE_ENV: &str = "PERSIA_DATABASE_CHILD_ROLE";
/// The database path a child works on.
const PATH_ENV: &str = "PERSIA_DATABASE_CHILD_PATH";
/// The seed of a child's schedule.
const SEED_ENV: &str = "PERSIA_DATABASE_CHILD_SEED";
/// The prefix of the ids a child writes: unique per child, so ids never repeat across kills.
const PREFIX_ENV: &str = "PERSIA_DATABASE_CHILD_PREFIX";
/// Prefix of the lines children report; anything else on their stdout is libtest's.
const TAG: &str = "@@persia ";
/// The one collection.
const COLLECTION: &str = "docs";

#[cfg(all(unix, not(feature = "slow")))]
const FILES: u64 = 1;
#[cfg(all(unix, feature = "slow"))]
const FILES: u64 = 10;

/// Kills per file: the checks after each kill cover every acknowledgement on the file so far.
#[cfg(unix)]
const KILLS_PER_FILE: u64 = 8;

fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_0603_db00);
    eprintln!("PERSIA_TEST_SEED={seed}");
    seed
}

/// `SplitMix64`: small, seedable, and good enough for test schedules.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform-ish in `lo..hi`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo)
    }
}

fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

/// The id of a child's `n`th put.
fn id_of(prefix: &str, n: u64) -> String {
    format!("{prefix}-{n}")
}

/// The body a put of `id` writes: a pure function of the id, so the parent can check any document. Lengths are
/// skewed short with an occasional multi-KB body.
fn body_of(id: &str) -> String {
    const VOCAB: [&str; 8] = [
        "alpha", "beta", "gamma", "delta", "persia", "busqueda", "índice", "segmento",
    ];
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for b in id.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let mut rng = Rng(h);
    let words = match rng.next_u64() % 10 {
        0 => rng.range(300, 1500),
        _ => rng.range(1, 30),
    };
    let mut out = id.to_owned();
    for _ in 0..words {
        out.push(' ');
        out.push_str(VOCAB[(rng.next_u64() % VOCAB.len() as u64) as usize]);
    }
    out
}

fn doc(id: &str) -> Document {
    let body = schema("x").field("body").unwrap().id();
    Document::new(id).with(body, Value::String(body_of(id)))
}

fn options(seed: u64) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)),
        segment_ids: SegmentIds::seeded(seed),
        // Small chunks, so the children's puts rotate WAL chunks too.
        wal_chunk_target: 4096,
        ..DatabaseOptions::default()
    }
}

/// The document with id `id` as `snapshot` shows it: the memtable's version hides the segments', and newer segments
/// hide older ones (SPEC §8.3).
fn get(snapshot: &Snapshot, id: &str) -> Option<Document> {
    let c = snapshot.collection(COLLECTION)?;
    let memtable = c.memtable();
    match memtable.lookup(id) {
        Some(IdState::Live(docid)) => return Some(memtable.document(docid).unwrap()),
        Some(IdState::Deleted) => return None,
        None => {}
    }
    c.segments()
        .iter()
        .rev()
        .find_map(|s| s.document_by_id(id).unwrap().map(|(_, d)| d))
}

/// Every live document id `snapshot` shows.
fn ids(snapshot: &Snapshot) -> BTreeSet<String> {
    let c = snapshot.collection(COLLECTION).unwrap();
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
    ids.into_iter()
        .filter(|id| get(snapshot, id).is_some())
        .collect()
}

/// Reads the child's stdout up to its next tagged line, returning it without the tag; `None` at EOF.
fn next_tagged(out: &mut BufReader<ChildStdout>) -> Option<String> {
    let mut line = String::new();
    loop {
        line.clear();
        if out.read_line(&mut line).unwrap() == 0 {
            return None;
        }
        // libtest prints `test child_role ... ` without a newline before the test runs, so the tag may follow it.
        // An unterminated line was cut by the child's death: what it reported is not acknowledged.
        if let Some(rest) = line.strip_suffix('\n')
            && let Some(msg) = rest.split_once(TAG).map(|(_, m)| m)
        {
            return Some(msg.to_owned());
        }
    }
}

/// Writes one tagged line to stdout and flushes it, bypassing libtest's capture.
fn report(msg: &str) {
    let mut out = std::io::stdout().lock();
    out.write_all(format!("{TAG}{msg}\n").as_bytes()).unwrap();
    out.flush().unwrap();
}

// ---------------------------------------------------------------------------------------------------------------
// Child role
// ---------------------------------------------------------------------------------------------------------------

/// The entry point of every child process; a no-op in a normal test run.
#[test]
fn child_role() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let path = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    let seed: u64 = std::env::var(SEED_ENV).unwrap().parse().unwrap();
    let prefix = std::env::var(PREFIX_ENV).unwrap();
    match role.as_str() {
        "writer" => child_writer(&path, seed, &prefix),
        other => panic!("unknown child role {other}"),
    }
}

/// Set once stdin reaches EOF: the parent holds the child's stdin until it reaped it, so EOF means the parent is
/// gone and the writer stops instead of running on (not a timer, see `container_process.rs`).
fn parent_gone() -> Arc<AtomicBool> {
    let gone = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&gone);
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let ended = std::io::stdin().read_to_end(&mut sink);
        flag.store(true, Ordering::Relaxed);
        ended.unwrap();
    });
    gone
}

/// Opens the database at `path` and puts documents `<prefix>-0`, `<prefix>-1`, ... with `Fsync`, committing every
/// seeded few puts, until killed or its stdin closes.
///
/// Reports `opened <seq>` after recovery, `acked <n>` once the put of `<prefix>-<n>` returned, and `commit <seq>`
/// once `commit()` returned.
fn child_writer(path: &Path, seed: u64, prefix: &str) {
    let mut rng = Rng(seed);
    // A process another test forks keeps copies of the parent's file descriptions until it execs, so a lock can
    // outlive its holder for a moment. Retry `Locked` briefly.
    let mut attempts = 0;
    let mut db = loop {
        match Database::open(path, options(seed)) {
            Err(DatabaseError::Container(ContainerError::Locked)) if attempts < 100_000 => {
                attempts += 1;
                std::thread::yield_now();
            }
            other => break other.unwrap(),
        }
    };
    report(&format!("opened {}", db.seq()));
    let commit_every = rng.range(1, 12);
    let gone = parent_gone();
    let mut n = 0;
    while !gone.load(Ordering::Relaxed) {
        db.put(COLLECTION, &doc(&id_of(prefix, n)), Durability::Fsync)
            .unwrap();
        report(&format!("acked {n}"));
        n += 1;
        if n % commit_every == 0 {
            let seq = db.commit().unwrap();
            report(&format!("commit {seq}"));
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The kill loop
// ---------------------------------------------------------------------------------------------------------------

/// What one killed child acknowledged.
#[cfg(unix)]
#[derive(Debug, Default)]
struct ChildAcks {
    /// The number of puts acknowledged: ids `0..puts`, in order.
    puts: u64,
    /// The last `seq` `commit()` returned.
    commit: Option<u64>,
}

#[cfg(unix)]
fn spawn_writer(path: &Path, seed: u64, prefix: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, "writer")
        .env(PATH_ENV, path)
        .env(SEED_ENV, seed.to_string())
        .env(PREFIX_ENV, prefix)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

/// Runs one writer child with ids `<prefix>-*` on `path` and SIGKILLs it after a seeded number of acknowledged puts
/// and a seeded delay.
#[cfg(unix)]
fn run_and_kill(path: &Path, rng: &mut Rng, prefix: &str, ctx: &str) -> ChildAcks {
    let child_seed = rng.next_u64();
    let kill_after = rng.range(1, 60);
    let delay = std::time::Duration::from_micros(rng.range(0, 3000));
    let mut child = spawn_writer(path, child_seed, prefix);
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let opened = next_tagged(&mut out).unwrap_or_else(|| panic!("{ctx}: writer died on open"));
    assert!(opened.starts_with("opened "), "{ctx}: {opened}");
    // A reader thread drains stdout so the child never blocks on a full pipe.
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        while let Some(msg) = next_tagged(&mut out) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    let mut acks = ChildAcks::default();
    let record = |msg: &str, acks: &mut ChildAcks| {
        let (verb, num) = msg.split_once(' ').unwrap();
        let num: u64 = num.parse().unwrap();
        match verb {
            "acked" => {
                assert_eq!(num, acks.puts, "{ctx}: acks out of order");
                acks.puts += 1;
            }
            "commit" => acks.commit = Some(num),
            other => panic!("{ctx}: unknown child report {other}"),
        }
    };
    loop {
        let Ok(msg) = rx.recv() else {
            let status = child.wait().unwrap();
            panic!(
                "{ctx}: writer exited before the kill after {} puts: {status}",
                acks.puts
            );
        };
        record(&msg, &mut acks);
        if acks.puts >= kill_after {
            std::thread::sleep(delay);
            break;
        }
    }
    // Held until the child is reaped, so it never sees EOF and stops by itself.
    let stdin = child.stdin.take();
    child.kill().unwrap(); // SIGKILL on Unix
    let status = child.wait().unwrap();
    drop(stdin);
    reader.join().unwrap();
    for msg in rx.try_iter() {
        record(&msg, &mut acks);
    }
    assert!(
        !status.success(),
        "{ctx}: writer exited by itself: {status}"
    );
    acks
}

/// Checks invariants 1-3 on `db`: every acknowledged put of every child (`prefix -> acks`) is present with its body,
/// nothing else is but each child's one in-flight put, and `seq` covers the last acknowledged commit. Returns the
/// number of documents.
#[cfg(unix)]
fn check(db: &Database, children: &BTreeMap<String, ChildAcks>, ctx: &str) -> usize {
    let snapshot = db.snapshot();
    let mut allowed = BTreeSet::new();
    for (prefix, acks) in children {
        for n in 0..acks.puts {
            let id = id_of(prefix, n);
            match get(&snapshot, &id) {
                Some(d) => assert!(d == doc(&id), "{ctx}: {id} has a wrong body: {d:?}"),
                None => panic!("{ctx}: acknowledged {id} is missing"),
            }
            allowed.insert(id);
        }
        // The put the kill may have cut after its fsync but before its report.
        allowed.insert(id_of(prefix, acks.puts));
    }
    let present = ids(&snapshot);
    for id in &present {
        assert!(allowed.contains(id), "{ctx}: phantom document {id}");
        assert!(
            get(&snapshot, id).unwrap() == doc(id),
            "{ctx}: {id} has a wrong body"
        );
    }
    let last_commit = children.values().filter_map(|a| a.commit).max();
    if let Some(seq) = last_commit {
        assert!(
            db.seq() >= seq,
            "{ctx}: seq {} is below the acknowledged commit {seq}",
            db.seq()
        );
    }
    present.len()
}

/// [`KILLS_PER_FILE`] killed writers on one new database; returns the acknowledged puts and the final document count.
#[cfg(unix)]
fn kill_loop_on_one_file(rng: &mut Rng, ctx: &str) -> (u64, usize) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    {
        let mut db = Database::create(&path, options(rng.next_u64())).unwrap();
        db.create_collection(schema(COLLECTION)).unwrap();
    }
    let mut children = BTreeMap::new();
    let mut docs = 0;
    for kill in 0..KILLS_PER_FILE {
        let ctx = format!("{ctx}, kill {kill}");
        let prefix = format!("k{kill}");
        let acks = run_and_kill(&path, rng, &prefix, &ctx);
        children.insert(prefix, acks);

        let mut db = Database::open(&path, options(rng.next_u64()))
            .unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
        let before = check(&db, &children, &format!("{ctx}, reopen"));
        let present = ids(&db.snapshot());
        db.commit()
            .unwrap_or_else(|e| panic!("{ctx}: commit after reopen failed: {e}"));
        assert_eq!(
            ids(&db.snapshot()),
            present,
            "{ctx}: commit changed documents"
        );
        drop(db);
        let db = Database::open(&path, options(rng.next_u64()))
            .unwrap_or_else(|e| panic!("{ctx}: second reopen failed: {e}"));
        docs = check(&db, &children, &format!("{ctx}, second reopen"));
        assert_eq!(docs, before, "{ctx}: second reopen changed the count");
        assert_eq!(
            ids(&db.snapshot()),
            present,
            "{ctx}: second reopen changed ids"
        );
    }
    let acked = children.values().map(|a| a.puts).sum();
    (acked, docs)
}

/// Writer children `put` with `Fsync` and commit on a real file and are killed with SIGKILL at seeded points; every
/// acknowledged document survives every reopen, and nothing unacknowledged but an in-flight put appears.
#[cfg(unix)]
#[test]
fn kill_9_loop_keeps_every_acknowledged_put() {
    let seed = base_seed();
    let mut rng = Rng(seed);
    let started = std::time::Instant::now();
    let (mut acked, mut docs) = (0, 0);
    for file in 0..FILES {
        let (a, d) = kill_loop_on_one_file(&mut rng, &format!("seed {seed}, file {file}"));
        acked += a;
        docs += d;
    }
    eprintln!(
        "database kill-9 loop: {} kills on {FILES} files, {acked} acknowledged puts, {docs} documents, {:?}",
        FILES * KILLS_PER_FILE,
        started.elapsed()
    );
}

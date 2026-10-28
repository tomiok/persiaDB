//! Real-file, real-process crash test of the commit path (ROADMAP 6.3, 6.4.3; SPEC §4.5, §8.2.1, §8.3, §8.6).
//!
//! A writer child (this test binary re-executed with [`ROLE_ENV`] set, as in `persia-storage`'s
//! `container_process.rs`) opens a database in a temp-dir container file and runs a seeded workload with
//! [`Durability::Fsync`]: puts of fresh ids, upserts (a new body) of live ids, deletes of live ids, deletes of ids
//! already deleted, and puts that bring a deleted id back. The ids it picks from include every id earlier children
//! wrote, so most upserts and deletes hit documents already in committed segments, and the commits write tombstone
//! sidecars (ROADMAP 6.4.3, ADR-0030). The child reports each operation on stdout before it starts it, and again once
//! it returned (acknowledged), and commits every few operations. The parent keeps a model of id -> body-or-deleted,
//! SIGKILLs the child after a seeded number of acknowledgements and a seeded delay, reopens the database with
//! [`Database::open`], and checks it; then the next child continues on the same file (crash after crash).
//!
//! Invariants, after every kill:
//! 1. The reopen succeeds, and every acknowledged operation's effect is visible: for each id, the latest
//!    acknowledged operation wins (SPEC §8.3, §8.6). The one operation the killed child had in flight (reported as
//!    started, not as acknowledged) may have taken effect or not; nothing else differs from the model.
//! 2. No phantom, no resurrection: every document present is live in the model with that body, or is the target of
//!    the killed child's in-flight put. In particular no deleted id is visible unless that put was of it.
//! 3. Segments agree with their tombstones: no id is live in two segments of the snapshot
//!    (`CollectionSnapshot::segment_views` with `is_live`), and each manifest segment entry is the snapshot's
//!    segment at the same index, with `tombstones.deleted` equal to the snapshot's `tombstones()[i].len()` (0 for
//!    none) and live plus deleted equal to `doc_count`.
//! 4. The committed `seq` is at least the last `seq` a child reported from `commit()`.
//! 5. Recovery is durable: `commit()` after the reopen, then another reopen, shows the same documents (with the
//!    in-flight operation resolved to what the first reopen showed), and invariants 1-3 hold after both.
//!
//! SIGKILL kills the process, not the machine: writes the child never synced stay in the page cache. So this tests
//! process-crash recovery (a put, delete or commit cut at any step, repeated recoveries of one file, the lock released
//! by the kernel); power loss is simulated on a `SimFile` by `database_crash.rs`.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed; a failure is replayed with
//! `PERSIA_TEST_SEED=<seed> cargo test -p persia-engine --test database_process`. The kill points also depend on
//! timing, so a replay reproduces the schedule (counts, delays, commit cadence, operation mix), not the exact instant
//! (and so not exactly which ids later children pick).

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
use persia_engine::manifest::SegmentId;
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
/// The prefix of the fresh ids a child writes and of its body tokens: unique per child, so they never repeat across
/// kills.
const PREFIX_ENV: &str = "PERSIA_DATABASE_CHILD_PREFIX";
/// The file listing the ids earlier children wrote (the parent's model): one `live <id>` or `deleted <id>` per line.
const KNOWN_ENV: &str = "PERSIA_DATABASE_CHILD_KNOWN";
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
const KILLS_PER_FILE: u64 = 10;

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

    /// A uniform-ish element of the non-empty `set`.
    fn pick<'a>(&mut self, set: &'a BTreeSet<String>) -> &'a String {
        let i = self.next_u64() % set.len() as u64;
        set.iter().nth(i as usize).unwrap()
    }
}

fn schema(name: &str) -> CollectionSchema {
    SchemaBuilder::new(name)
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap()
}

/// The id of a child's `n`th fresh put.
fn id_of(prefix: &str, n: u64) -> String {
    format!("{prefix}-{n}")
}

/// The body a put of `id` with body token `token` writes: a pure function of both, so the parent can check any
/// document from the reported operation, and an upsert's body differs from the one it replaces. Lengths are skewed
/// short with an occasional multi-KB body.
fn body_of(id: &str, token: &str) -> String {
    const VOCAB: [&str; 8] = [
        "alpha", "beta", "gamma", "delta", "persia", "busqueda", "índice", "segmento",
    ];
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for b in id.bytes().chain(*b"#").chain(token.bytes()) {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let mut rng = Rng(h);
    let words = match rng.next_u64() % 10 {
        0 => rng.range(300, 1500),
        _ => rng.range(1, 30),
    };
    let mut out = format!("{id} {token}");
    for _ in 0..words {
        out.push(' ');
        out.push_str(VOCAB[(rng.next_u64() % VOCAB.len() as u64) as usize]);
    }
    out
}

fn doc(id: &str, token: &str) -> Document {
    let body = schema("x").field("body").unwrap().id();
    Document::new(id).with(body, Value::String(body_of(id, token)))
}

fn options(seed: u64) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)),
        segment_ids: SegmentIds::seeded(seed),
        // Small chunks, so the children's writes rotate WAL chunks too.
        wal_chunk_target: 4096,
        ..DatabaseOptions::default()
    }
}

/// One write of the workload.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    /// A put (insert or upsert) of `id` with the body of `token`.
    Put { id: String, token: String },
    /// A delete of `id`.
    Delete { id: String },
}

impl Op {
    fn id(&self) -> &str {
        match self {
            Self::Put { id, .. } | Self::Delete { id } => id,
        }
    }

    /// The model state of its id after it: the body token, or `None` for deleted.
    fn effect(&self) -> Option<String> {
        match self {
            Self::Put { token, .. } => Some(token.clone()),
            Self::Delete { .. } => None,
        }
    }

    /// `put <id> <token>` or `delete <id>`; ids and tokens have no spaces.
    fn encode(&self) -> String {
        match self {
            Self::Put { id, token } => format!("put {id} {token}"),
            Self::Delete { id } => format!("delete {id}"),
        }
    }

    fn decode(s: &str) -> Self {
        let parts: Vec<&str> = s.split(' ').collect();
        match parts.as_slice() {
            ["put", id, token] => Self::Put {
                id: (*id).to_owned(),
                token: (*token).to_owned(),
            },
            ["delete", id] => Self::Delete {
                id: (*id).to_owned(),
            },
            _ => panic!("bad op {s:?}"),
        }
    }
}

/// The parent's model: id -> the body token of its live version, or `None` once deleted.
type Model = BTreeMap<String, Option<String>>;

/// The document with id `id` as `snapshot` shows it: the memtable's version or delete hides the segments', then the
/// newest segment that holds the id decides (SPEC §8.3).
fn get(snapshot: &Snapshot, id: &str) -> Option<Document> {
    let c = snapshot.collection(COLLECTION)?;
    c.get(id).unwrap()
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

/// Every live document of `snapshot`, by id.
fn documents(snapshot: &Snapshot) -> BTreeMap<String, Document> {
    ids(snapshot)
        .into_iter()
        .map(|id| {
            let d = get(snapshot, &id).unwrap();
            (id, d)
        })
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
    let known = PathBuf::from(std::env::var(KNOWN_ENV).unwrap());
    match role.as_str() {
        "writer" => child_writer(&path, seed, &prefix, &known),
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

/// Reads the parent's model file: the live and the deleted ids.
fn read_known(path: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut live, mut deleted) = (BTreeSet::new(), BTreeSet::new());
    for line in std::fs::read_to_string(path).unwrap().lines() {
        match line.split_once(' ') {
            Some(("live", id)) => live.insert(id.to_owned()),
            Some(("deleted", id)) => deleted.insert(id.to_owned()),
            _ => panic!("bad known line {line:?}"),
        };
    }
    (live, deleted)
}

/// The next operation of the workload: a fresh put (`<prefix>-<fresh>`, 35%), an upsert (25%) or delete (25%) of a
/// live id, a delete of a deleted id (5%), or a put of a deleted id (10%); a fresh put when the pick has no
/// candidate. `token` is its body token.
fn next_op(
    rng: &mut Rng,
    live: &BTreeSet<String>,
    deleted: &BTreeSet<String>,
    prefix: &str,
    fresh: &mut u64,
    token: String,
) -> Op {
    let op = match rng.next_u64() % 100 {
        35..60 if !live.is_empty() => Some(Op::Put {
            id: rng.pick(live).clone(),
            token: token.clone(),
        }),
        60..85 if !live.is_empty() => Some(Op::Delete {
            id: rng.pick(live).clone(),
        }),
        85..90 if !deleted.is_empty() => Some(Op::Delete {
            id: rng.pick(deleted).clone(),
        }),
        90..100 if !deleted.is_empty() => Some(Op::Put {
            id: rng.pick(deleted).clone(),
            token: token.clone(),
        }),
        _ => None,
    };
    op.unwrap_or_else(|| {
        let id = id_of(prefix, *fresh);
        *fresh += 1;
        Op::Put { id, token }
    })
}

/// Opens the database at `path` and runs the seeded workload of [`next_op`] with `Fsync` over the ids of `known`
/// and its own fresh ids `<prefix>-0`, `<prefix>-1`, ..., committing every seeded few operations, until killed or its
/// stdin closes.
///
/// Reports `opened <seq>` after recovery; `op <n> <op>` before it starts its `n`th operation and `acked <n>` once it
/// returned; and `commit <seq>` once `commit()` returned.
fn child_writer(path: &Path, seed: u64, prefix: &str, known: &Path) {
    let mut rng = Rng(seed);
    let (mut live, mut deleted) = read_known(known);
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
    let (mut n, mut fresh) = (0, 0);
    while !gone.load(Ordering::Relaxed) {
        let token = format!("{prefix}.{n}");
        let op = next_op(&mut rng, &live, &deleted, prefix, &mut fresh, token);
        report(&format!("op {n} {}", op.encode()));
        match &op {
            Op::Put { id, token } => {
                db.put(COLLECTION, &doc(id, token), Durability::Fsync)
                    .unwrap();
                deleted.remove(id);
                live.insert(id.clone());
            }
            Op::Delete { id } => {
                db.delete(COLLECTION, id, Durability::Fsync).unwrap();
                live.remove(id);
                deleted.insert(id.clone());
            }
        }
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

/// What one killed child reported.
#[cfg(unix)]
#[derive(Debug, Default)]
struct ChildRun {
    /// The acknowledged operations, in order.
    acked: Vec<Op>,
    /// The operation it reported as started but not as acknowledged: it may or may not have taken effect.
    in_flight: Option<Op>,
    /// The last `seq` `commit()` returned.
    commit: Option<u64>,
}

#[cfg(unix)]
fn spawn_writer(path: &Path, seed: u64, prefix: &str, known: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, "writer")
        .env(PATH_ENV, path)
        .env(SEED_ENV, seed.to_string())
        .env(PREFIX_ENV, prefix)
        .env(KNOWN_ENV, known)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

/// Writes `model` for the next child to pick ids from.
#[cfg(unix)]
fn write_known(path: &Path, model: &Model) {
    let mut out = String::new();
    for (id, state) in model {
        let kind = if state.is_some() { "live" } else { "deleted" };
        out.push_str(kind);
        out.push(' ');
        out.push_str(id);
        out.push('\n');
    }
    std::fs::write(path, out).unwrap();
}

/// Runs one writer child on `path` (fresh ids `<prefix>-*`, existing ids picked from `known`) and SIGKILLs it after
/// a seeded number of acknowledged operations and a seeded delay.
#[cfg(unix)]
fn run_and_kill(path: &Path, known: &Path, rng: &mut Rng, prefix: &str, ctx: &str) -> ChildRun {
    let child_seed = rng.next_u64();
    let kill_after = rng.range(1, 60);
    let delay = std::time::Duration::from_micros(rng.range(0, 3000));
    let mut child = spawn_writer(path, child_seed, prefix, known);
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
    let mut run = ChildRun::default();
    let record = |msg: &str, run: &mut ChildRun| {
        let (verb, rest) = msg.split_once(' ').unwrap();
        match verb {
            "op" => {
                let (n, op) = rest.split_once(' ').unwrap();
                let n: usize = n.parse().unwrap();
                assert_eq!(n, run.acked.len(), "{ctx}: ops out of order");
                assert!(run.in_flight.is_none(), "{ctx}: two ops in flight");
                run.in_flight = Some(Op::decode(op));
            }
            "acked" => {
                let n: usize = rest.parse().unwrap();
                assert_eq!(n, run.acked.len(), "{ctx}: acks out of order");
                let op = run
                    .in_flight
                    .take()
                    .unwrap_or_else(|| panic!("{ctx}: ack {n} of no op"));
                run.acked.push(op);
            }
            "commit" => run.commit = Some(rest.parse().unwrap()),
            other => panic!("{ctx}: unknown child report {other}"),
        }
    };
    loop {
        let Ok(msg) = rx.recv() else {
            let status = child.wait().unwrap();
            panic!(
                "{ctx}: writer exited before the kill after {} ops: {status}",
                run.acked.len()
            );
        };
        record(&msg, &mut run);
        if run.acked.len() as u64 >= kill_after {
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
        record(&msg, &mut run);
    }
    assert!(
        !status.success(),
        "{ctx}: writer exited by itself: {status}"
    );
    run
}

/// The document a model state stands for: `None` for deleted or never written.
fn expected(id: &str, state: Option<&Option<String>>) -> Option<Document> {
    state.and_then(|s| s.as_deref()).map(|token| doc(id, token))
}

fn describe(d: Option<&Document>) -> String {
    match d {
        None => "absent".to_owned(),
        Some(d) => format!("{d:?}").chars().take(200).collect(),
    }
}

/// Checks invariant 3 on `snapshot`: no id is live in two segments, and the manifest's segment entries are the
/// snapshot's segments with their tombstone counts.
#[cfg(unix)]
fn check_segments(snapshot: &Snapshot, ctx: &str) {
    let c = snapshot.collection(COLLECTION).unwrap();
    let entries = snapshot
        .manifest()
        .collection(COLLECTION)
        .unwrap_or_else(|| panic!("{ctx}: no manifest entry for {COLLECTION}"))
        .segments();
    assert_eq!(
        entries.len(),
        c.segments().len(),
        "{ctx}: manifest and snapshot segment counts differ"
    );
    assert_eq!(c.tombstones().len(), c.segments().len(), "{ctx}");
    let mut owner: BTreeMap<String, usize> = BTreeMap::new();
    for (i, view) in c.segment_views().enumerate() {
        let entry = &entries[i];
        assert_eq!(
            entry.id,
            SegmentId::new(view.segment().segment_id()),
            "{ctx}: segment {i} differs between manifest and snapshot"
        );
        let in_snapshot = c.tombstones()[i].as_ref().map_or(0, |t| t.len());
        let in_manifest = entry.tombstones.map_or(0, |t| u64::from(t.deleted));
        assert_eq!(
            in_manifest, in_snapshot,
            "{ctx}: segment {i} ({}): manifest says {in_manifest} deleted, sidecar has {in_snapshot}",
            entry.id
        );
        let mut live = 0_u64;
        for docid in 0..view.doc_count() {
            if view.is_live(docid) {
                live += 1;
                let id = view.segment().id_of(docid).unwrap();
                if let Some(j) = owner.insert(id.clone(), i) {
                    panic!("{ctx}: {id} is live in segments {j} and {i}");
                }
            }
        }
        assert_eq!(
            live + in_snapshot,
            u64::from(entry.doc_count),
            "{ctx}: segment {i}: live + deleted != doc_count"
        );
    }
}

/// Checks invariants 1-4 on `db` against `model` (every acknowledged operation applied), `in_flight` (the killed
/// child's unacknowledged operation, if any) and `last_commit`. Returns the state the in-flight operation's id has in
/// `db`, to update the model with (`None` if it is absent and not in the model).
#[cfg(unix)]
fn check(
    db: &Database,
    model: &Model,
    in_flight: Option<&Op>,
    last_commit: Option<u64>,
    ctx: &str,
) -> Option<(String, Option<String>)> {
    let snapshot = db.snapshot();
    for (id, state) in model {
        let actual = get(&snapshot, id);
        let want = expected(id, Some(state));
        let by_in_flight = in_flight
            .filter(|op| op.id() == id)
            .is_some_and(|op| actual == expected(id, Some(&op.effect())));
        assert!(
            actual == want || by_in_flight,
            "{ctx}: {id} should be {}, but is {}",
            describe(want.as_ref()),
            describe(actual.as_ref())
        );
    }
    let resolved = in_flight.and_then(|op| {
        let actual = get(&snapshot, op.id());
        let before = model.get(op.id());
        if actual == expected(op.id(), Some(&op.effect())) {
            Some((op.id().to_owned(), op.effect()))
        } else {
            assert!(
                actual == expected(op.id(), before),
                "{ctx}: in-flight {op:?} left {}",
                describe(actual.as_ref())
            );
            before.map(|s| (op.id().to_owned(), s.clone()))
        }
    });
    for id in ids(&snapshot) {
        let live_in_model = matches!(model.get(&id), Some(Some(_)));
        let in_flight_put = matches!(in_flight, Some(Op::Put { id: i, .. }) if *i == id);
        assert!(
            live_in_model || in_flight_put,
            "{ctx}: {id} is visible but {}",
            if model.contains_key(&id) {
                "deleted"
            } else {
                "never written (phantom)"
            }
        );
    }
    check_segments(&snapshot, ctx);
    if let Some(seq) = last_commit {
        assert!(
            db.seq() >= seq,
            "{ctx}: seq {} is below the acknowledged commit {seq}",
            db.seq()
        );
    }
    resolved
}

/// Counts over the acknowledged operations of the run.
#[cfg(unix)]
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    inserts: u64,
    upserts: u64,
    deletes: u64,
    /// Upserts and deletes of ids an earlier child wrote: committed, or replayed from the WAL on open.
    of_earlier: u64,
    /// Live documents after the last kill.
    docs: usize,
    /// Segments with a tombstone sidecar after the last kill.
    sidecars: usize,
}

/// [`KILLS_PER_FILE`] killed writers on one new database.
#[cfg(unix)]
fn kill_loop_on_one_file(rng: &mut Rng, ctx: &str) -> Stats {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let known = dir.path().join("known.txt");
    {
        let mut db = Database::create(&path, options(rng.next_u64())).unwrap();
        db.create_collection(schema(COLLECTION)).unwrap();
    }
    let mut model = Model::new();
    let mut last_commit = None;
    let mut stats = Stats::default();
    for kill in 0..KILLS_PER_FILE {
        let ctx = format!("{ctx}, kill {kill}");
        let prefix = format!("k{kill}");
        write_known(&known, &model);
        let earlier: BTreeSet<String> = model.keys().cloned().collect();
        let run = run_and_kill(&path, &known, rng, &prefix, &ctx);
        for op in &run.acked {
            match op {
                Op::Put { id, .. } if model.get(id).is_some_and(Option::is_some) => {
                    stats.upserts += 1;
                }
                Op::Put { .. } => stats.inserts += 1,
                Op::Delete { .. } => stats.deletes += 1,
            }
            if earlier.contains(op.id()) {
                stats.of_earlier += 1;
            }
            model.insert(op.id().to_owned(), op.effect());
        }
        last_commit = last_commit.max(run.commit);

        let mut db = Database::open(&path, options(rng.next_u64()))
            .unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
        let reopen_ctx = format!("{ctx}, reopen (in flight: {:?})", run.in_flight);
        let resolved = check(
            &db,
            &model,
            run.in_flight.as_ref(),
            last_commit,
            &reopen_ctx,
        );
        if let Some((id, state)) = resolved {
            model.insert(id, state);
        }
        let present = documents(&db.snapshot());
        let seq = db
            .commit()
            .unwrap_or_else(|e| panic!("{ctx}: commit after reopen failed: {e}"));
        last_commit = last_commit.max(Some(seq));
        assert!(
            documents(&db.snapshot()) == present,
            "{ctx}: commit changed documents"
        );
        check(&db, &model, None, last_commit, &format!("{ctx}, commit"));
        drop(db);
        let db = Database::open(&path, options(rng.next_u64()))
            .unwrap_or_else(|e| panic!("{ctx}: second reopen failed: {e}"));
        let second_ctx = format!("{ctx}, second reopen");
        check(&db, &model, None, last_commit, &second_ctx);
        let snapshot = db.snapshot();
        assert!(
            documents(&snapshot) == present,
            "{ctx}: second reopen changed documents"
        );
        stats.docs = present.len();
        stats.sidecars = snapshot
            .collection(COLLECTION)
            .unwrap()
            .tombstones()
            .iter()
            .flatten()
            .count();
    }
    stats
}

/// Writer children put, upsert and delete with `Fsync` and commit on a real file, also over ids earlier children
/// committed, and are killed with SIGKILL at seeded points; every acknowledged operation survives every reopen, no
/// deleted id comes back, and nothing unacknowledged but an in-flight operation shows.
#[cfg(unix)]
#[test]
fn kill_9_loop_keeps_every_acknowledged_write() {
    let seed = base_seed();
    let mut rng = Rng(seed);
    let started = std::time::Instant::now();
    let mut total = Stats::default();
    for file in 0..FILES {
        let s = kill_loop_on_one_file(&mut rng, &format!("seed {seed}, file {file}"));
        total.inserts += s.inserts;
        total.upserts += s.upserts;
        total.deletes += s.deletes;
        total.of_earlier += s.of_earlier;
        total.docs += s.docs;
        total.sidecars += s.sidecars;
    }
    eprintln!(
        "database kill-9 loop: {} kills on {FILES} files; acknowledged {} inserts, {} upserts, {} deletes \
         ({} on ids of earlier children); {} documents and {} tombstone sidecars at the end; {:?}",
        FILES * KILLS_PER_FILE,
        total.inserts,
        total.upserts,
        total.deletes,
        total.of_earlier,
        total.docs,
        total.sidecars,
        started.elapsed()
    );
}

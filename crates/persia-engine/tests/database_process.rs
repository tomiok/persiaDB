//! Real-file, real-process crash tests of the commit path and the open sequence (ROADMAP 6.3, 6.4.3, 6.6.4; SPEC
//! §4.5, §8.2.1, §8.3, §8.6; ADR-0033).
//!
//! A writer child (this test binary re-executed with [`ROLE_ENV`] set, as in `persia-storage`'s
//! `container_process.rs`) opens a database in a temp-dir container file and runs a seeded workload with
//! [`Durability::Fsync`] over one or more collections: puts of fresh ids, upserts (a new body) of live ids, deletes of
//! live ids, deletes of ids already deleted, and puts that bring a deleted id back. The ids it picks from include
//! every id earlier children wrote, so most upserts and deletes hit documents already in committed segments, and the
//! commits write tombstone sidecars (ROADMAP 6.4.3, ADR-0030). The child reports `opening` before
//! [`Database::open`] and `opened` once it returned, each operation on stdout before it starts it and again once it
//! returned (acknowledged), and commits every few operations. The parent keeps a model of (collection, id) ->
//! body-or-deleted, SIGKILLs the child at a seeded point, checks the file, and the next child continues on the same
//! file (crash after crash).
//!
//! Two scenarios:
//! - [`kill_9_loop_keeps_every_acknowledged_write`]: one collection, the default memtable threshold, every child
//!   commits every few operations, and the parent commits after each reopen. The commit path under kills.
//! - [`kill_9_loop_through_replay_flushes_merges_and_open`] (ROADMAP 6.6.4): two collections with skewed traffic, so
//!   one collection's unflushed record holds `wal_start` back for the other (SPEC §8.2.1). Children pick a memtable
//!   threshold (`CommitPolicy.flush.memtable_bytes`): the 64 MiB default (a backlog builds up, some children never
//!   commit) or a few KiB, so the next open's replay flushes (ROADMAP 6.6.5); the parent leaves the backlog in place
//!   after most reopens. Some children run merges (`merge_step`, 0 or 1 merge threads) right after the open and
//!   between operations. Some children are killed a seeded delay after `opening`, before or inside the open, so kills
//!   land in replay-time commits.
//!
//! Invariants, after every kill:
//! 0. A read-only open ([`ReadOnlyDatabase::open`], SPEC §8.6) succeeds and `verify()` passes, before any read-write
//!    open (which may commit); it shows the same documents as the read-write reopen of invariant 1, and its `seq` is
//!    at least that of invariant 4. While the child is alive (at a seeded acknowledgement), a read-only open succeeds
//!    and `verify()` passes, a second writer's [`Database::open`] fails with `Locked`, and every document the reader
//!    shows is either the state of the model plus the operations acknowledged before the reader opened, or the effect
//!    of an operation the child started after that.
//! 1. The reopen succeeds, and every acknowledged operation's effect is visible: for each key, the latest
//!    acknowledged operation wins (SPEC §8.3, §8.6). The one operation the killed child had in flight (reported as
//!    started, not as acknowledged) may have taken effect or not; nothing else differs from the model.
//! 2. No phantom, no resurrection: every document present is live in the model with that body, or is the target of
//!    the killed child's in-flight put. In particular no deleted id is visible unless that put was of it.
//! 3. Segments agree with their tombstones, per collection: no id is live in two segments of the snapshot
//!    (`CollectionSnapshot::segment_views` with `is_live`), and each manifest segment entry is the snapshot's
//!    segment at the same index, with `tombstones.deleted` equal to the snapshot's `tombstones()[i].len()` (0 for
//!    none) and live plus deleted equal to `doc_count`.
//! 4. The committed `seq` is at least the last `seq` a child reported from `commit()` or a merge commit.
//! 5. Recovery is durable: `commit()` after the reopen (always in the first scenario, sometimes in the second), then
//!    another reopen, shows the same documents (with the in-flight operation resolved to what the first reopen
//!    showed), and invariants 1-3 hold after both; a read-only `verify()` beside that last reopen passes.
//!
//! SIGKILL kills the process, not the machine: writes the child never synced stay in the page cache. So this tests
//! process-crash recovery (a put, delete, commit, merge commit or replay-time commit cut at any step, repeated
//! recoveries of one file, the lock released by the kernel); power loss is simulated on a `SimFile` by
//! `database_crash.rs` and `database_merge_crash.rs`.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed; a failure is replayed with
//! `PERSIA_TEST_SEED=<seed> cargo test -p persia-engine --test database_process`. The kill points also depend on
//! timing, so a replay reproduces the schedule (counts, delays, thresholds, commit and merge cadence, operation
//! mix), not the exact instant (and so not exactly which ids later children pick). `PERSIA_TEST_FILES` and
//! `PERSIA_TEST_KILLS` override the number of files and kills per file (long manual runs); `PERSIA_TEST_VERBOSE`
//! prints each kill's schedule. On a failure the file's temp dir is kept (its path is printed), with
//! `after-kill.persia`, the file as the last kill left it, before any check opened it.
//!
//! The children run this test binary from its path: rebuilding it (or the engine) while the test runs makes the
//! later children run other code than the parent.

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
use std::time::Duration;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    CommitPolicy, Database, DatabaseError, DatabaseOptions, ManualClock, MergeOutcome,
    ReadOnlyDatabase, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::manifest::SegmentId;
use persia_engine::memtable::FlushPolicy;
use persia_engine::merge::{MergeOptions, MergePolicy};
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
/// The file listing the keys earlier children wrote (the parent's model): one `live <collection> <id>` or
/// `deleted <collection> <id>` per line.
const KNOWN_ENV: &str = "PERSIA_DATABASE_CHILD_KNOWN";
/// The child's [`ChildConfig`], encoded.
const CONFIG_ENV: &str = "PERSIA_DATABASE_CHILD_CONFIG";
/// Prefix of the lines children report; anything else on their stdout is libtest's.
const TAG: &str = "@@persia ";

#[cfg(all(unix, not(feature = "slow")))]
const FILES: u64 = 1;
#[cfg(all(unix, feature = "slow"))]
const FILES: u64 = 10;

/// Kills per file: the checks after each kill cover every acknowledgement on the file so far.
#[cfg(unix)]
const KILLS_PER_FILE: u64 = 10;

/// Kills per file of the replay/merge/open scenario.
#[cfg(all(unix, not(feature = "slow")))]
const RECOVERY_KILLS_PER_FILE: u64 = 9;
#[cfg(all(unix, feature = "slow"))]
const RECOVERY_KILLS_PER_FILE: u64 = 40;

fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_0603_db00);
    eprintln!("PERSIA_TEST_SEED={seed}");
    seed
}

/// A count from the environment variable `var` (for long manual or nightly runs), else `default`; printed.
#[cfg(unix)]
fn env_count(var: &str, default: u64) -> u64 {
    let n = std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default);
    eprintln!("{var}={n}");
    n
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

    /// True with probability about `percent`%.
    fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
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

/// Options of every open: a fixed clock (so only the size threshold commits on its own), seeded segment ids, small
/// WAL chunks (so writes rotate chunks, and a replay-time commit's `wal_start` cap at the chunk matters), and merges
/// of two tier-0 segments (so a few flushes already plan one).
fn options(seed: u64, memtable_bytes: usize, merge_threads: usize) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)),
        segment_ids: SegmentIds::seeded(seed),
        wal_chunk_target: 4096,
        policy: CommitPolicy {
            flush: FlushPolicy {
                memtable_bytes,
                ..FlushPolicy::default()
            },
            ..CommitPolicy::default()
        },
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: merge_threads,
        },
        ..DatabaseOptions::default()
    }
}

/// A (collection, id) pair.
type Key = (String, String);

/// One write of the workload.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    /// A put (insert or upsert) of `id` with the body of `token`.
    Put {
        collection: String,
        id: String,
        token: String,
    },
    /// A delete of `id`.
    Delete { collection: String, id: String },
}

impl Op {
    fn key(&self) -> Key {
        match self {
            Self::Put { collection, id, .. } | Self::Delete { collection, id } => {
                (collection.clone(), id.clone())
            }
        }
    }

    /// The model state of its key after it: the body token, or `None` for deleted.
    fn effect(&self) -> Option<String> {
        match self {
            Self::Put { token, .. } => Some(token.clone()),
            Self::Delete { .. } => None,
        }
    }

    /// `put <collection> <id> <token>` or `delete <collection> <id>`; none of them has spaces.
    fn encode(&self) -> String {
        match self {
            Self::Put {
                collection,
                id,
                token,
            } => format!("put {collection} {id} {token}"),
            Self::Delete { collection, id } => format!("delete {collection} {id}"),
        }
    }

    fn decode(s: &str) -> Self {
        let parts: Vec<&str> = s.split(' ').collect();
        match parts.as_slice() {
            ["put", c, id, token] => Self::Put {
                collection: (*c).to_owned(),
                id: (*id).to_owned(),
                token: (*token).to_owned(),
            },
            ["delete", c, id] => Self::Delete {
                collection: (*c).to_owned(),
                id: (*id).to_owned(),
            },
            _ => panic!("bad op {s:?}"),
        }
    }
}

/// The parent's model: key -> the body token of its live version, or `None` once deleted.
type Model = BTreeMap<Key, Option<String>>;

/// What a child does, chosen by the parent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildConfig {
    collections: Vec<String>,
    memtable_bytes: usize,
    /// Merge threads, or `None` for no merges.
    merge_threads: Option<usize>,
    /// `merge_step` after every this many operations (and once right after the open).
    merge_every: u64,
    /// `commit()` after every this many operations; 0 for never.
    commit_every: u64,
}

impl ChildConfig {
    fn encode(&self) -> String {
        let threads = self
            .merge_threads
            .map_or_else(|| "-".to_owned(), |t| t.to_string());
        format!(
            "{} {} {threads} {} {}",
            self.collections.join(","),
            self.memtable_bytes,
            self.merge_every,
            self.commit_every
        )
    }

    fn decode(s: &str) -> Self {
        let parts: Vec<&str> = s.split(' ').collect();
        let [
            collections,
            memtable_bytes,
            threads,
            merge_every,
            commit_every,
        ] = parts.as_slice()
        else {
            panic!("bad child config {s:?}");
        };
        Self {
            collections: collections.split(',').map(str::to_owned).collect(),
            memtable_bytes: memtable_bytes.parse().unwrap(),
            merge_threads: (*threads != "-").then(|| threads.parse().unwrap()),
            merge_every: merge_every.parse().unwrap(),
            commit_every: commit_every.parse().unwrap(),
        }
    }
}

/// The document with key `(collection, id)` as `snapshot` shows it: the memtable's version or delete hides the
/// segments', then the newest segment that holds the id decides (SPEC §8.3).
fn get(snapshot: &Snapshot, collection: &str, id: &str) -> Option<Document> {
    let c = snapshot.collection(collection)?;
    c.get(id).unwrap()
}

/// Every live document id of `collection` in `snapshot`.
fn ids(snapshot: &Snapshot, collection: &str) -> BTreeSet<String> {
    let c = snapshot.collection(collection).unwrap();
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
        .filter(|id| get(snapshot, collection, id).is_some())
        .collect()
}

/// Every live document of `snapshot`, by key; every collection must exist.
fn documents(snapshot: &Snapshot, collections: &[&str]) -> BTreeMap<Key, Document> {
    let mut out = BTreeMap::new();
    for c in collections {
        for id in ids(snapshot, c) {
            let d = get(snapshot, c, &id).unwrap();
            out.insert(((*c).to_owned(), id), d);
        }
    }
    out
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
    let config = ChildConfig::decode(&std::env::var(CONFIG_ENV).unwrap());
    match role.as_str() {
        "writer" => child_writer(&path, seed, &prefix, &known, &config),
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

/// The live and the deleted ids of one collection.
#[derive(Debug, Default)]
struct Ids {
    live: BTreeSet<String>,
    deleted: BTreeSet<String>,
}

/// Reads the parent's model file: the live and the deleted ids of each of `collections`.
fn read_known(path: &Path, collections: &[String]) -> BTreeMap<String, Ids> {
    let mut out: BTreeMap<String, Ids> = collections
        .iter()
        .map(|c| (c.clone(), Ids::default()))
        .collect();
    for line in std::fs::read_to_string(path).unwrap().lines() {
        let parts: Vec<&str> = line.split(' ').collect();
        let [kind, c, id] = parts.as_slice() else {
            panic!("bad known line {line:?}");
        };
        let ids = out.get_mut(*c).unwrap();
        match *kind {
            "live" => ids.live.insert((*id).to_owned()),
            "deleted" => ids.deleted.insert((*id).to_owned()),
            _ => panic!("bad known line {line:?}"),
        };
    }
    out
}

/// The next operation of the workload. The collection: the first one three times as often as each other (with two,
/// 75% / 25%), so one collection's unflushed records sit among many of the other's. Then on it: a fresh put
/// (`<prefix>-<fresh>`, 35%), an upsert (25%) or delete (25%) of a live id, a delete of a deleted id (5%), or a put
/// of a deleted id (10%); a fresh put when the pick has no candidate. `token` is its body token.
fn next_op(
    rng: &mut Rng,
    known: &BTreeMap<String, Ids>,
    collections: &[String],
    prefix: &str,
    fresh: &mut u64,
    token: String,
) -> Op {
    let at = (rng.next_u64() % (collections.len() as u64 + 2)).saturating_sub(2);
    let collection = collections[at as usize].clone();
    let Ids { live, deleted } = &known[&collection];
    let op = match rng.next_u64() % 100 {
        35..60 if !live.is_empty() => Some(Op::Put {
            collection: collection.clone(),
            id: rng.pick(live).clone(),
            token: token.clone(),
        }),
        60..85 if !live.is_empty() => Some(Op::Delete {
            collection: collection.clone(),
            id: rng.pick(live).clone(),
        }),
        85..90 if !deleted.is_empty() => Some(Op::Delete {
            collection: collection.clone(),
            id: rng.pick(deleted).clone(),
        }),
        90..100 if !deleted.is_empty() => Some(Op::Put {
            collection: collection.clone(),
            id: rng.pick(deleted).clone(),
            token: token.clone(),
        }),
        _ => None,
    };
    op.unwrap_or_else(|| {
        let id = id_of(prefix, *fresh);
        *fresh += 1;
        Op::Put {
            collection,
            id,
            token,
        }
    })
}

/// Runs `merge_step` and reports each merge commit as `merge <seq>`. A failed merge means a damaged segment: the child
/// panics, and the parent sees it exit before the kill.
fn merge_step(db: &mut Database) {
    for outcome in db.merge_step().unwrap() {
        match outcome {
            MergeOutcome::Committed { seq, .. } => report(&format!("merge {seq}")),
            MergeOutcome::Failed {
                collection,
                inputs,
                error,
            } => panic!("merge of {inputs:?} in {collection} failed: {error}"),
            MergeOutcome::Conflict { .. } | MergeOutcome::Cancelled { .. } => {}
        }
    }
}

/// Opens the database at `path` and runs the seeded workload of [`next_op`] with `Fsync` over the keys of `known`
/// and its own fresh ids `<prefix>-0`, `<prefix>-1`, ..., committing and merging as `config` says, until killed or
/// its stdin closes.
///
/// Reports `opening` before the open and `opened <seq> <micros>` after it (the open's duration); `op <n> <op>`
/// before it starts its `n`th operation and `acked <n>` once it returned; `commit <seq>` once `commit()` returned
/// and `merge <seq>` for each merge commit.
fn child_writer(path: &Path, seed: u64, prefix: &str, known: &Path, config: &ChildConfig) {
    let mut rng = Rng(seed);
    let mut known = read_known(known, &config.collections);
    let opts = || {
        options(
            seed,
            config.memtable_bytes,
            config.merge_threads.unwrap_or(0),
        )
    };
    report("opening");
    let started = std::time::Instant::now();
    // A process another test forks keeps copies of the parent's file descriptions until it execs, so a lock can
    // outlive its holder for a moment. Retry `Locked` briefly.
    let mut attempts = 0;
    let mut db = loop {
        match Database::open(path, opts()) {
            Err(DatabaseError::Container(ContainerError::Locked)) if attempts < 100_000 => {
                attempts += 1;
                std::thread::yield_now();
            }
            other => break other.unwrap(),
        }
    };
    report(&format!(
        "opened {} {}",
        db.seq(),
        started.elapsed().as_micros()
    ));
    if config.merge_threads.is_some() {
        // Before any write: a merge commit flushes no memtable, so only the replayed records hold `wal_start`.
        merge_step(&mut db);
    }
    let gone = parent_gone();
    let (mut n, mut fresh) = (0, 0);
    while !gone.load(Ordering::Relaxed) {
        let token = format!("{prefix}.{n}");
        let op = next_op(
            &mut rng,
            &known,
            &config.collections,
            prefix,
            &mut fresh,
            token,
        );
        report(&format!("op {n} {}", op.encode()));
        match &op {
            Op::Put {
                collection,
                id,
                token,
            } => {
                db.put(collection, &doc(id, token), Durability::Fsync)
                    .unwrap();
                let ids = known.get_mut(collection).unwrap();
                ids.deleted.remove(id);
                ids.live.insert(id.clone());
            }
            Op::Delete { collection, id } => {
                db.delete(collection, id, Durability::Fsync).unwrap();
                let ids = known.get_mut(collection).unwrap();
                ids.live.remove(id);
                ids.deleted.insert(id.clone());
            }
        }
        report(&format!("acked {n}"));
        n += 1;
        if config.commit_every > 0 && n % config.commit_every == 0 {
            let seq = db.commit().unwrap();
            report(&format!("commit {seq}"));
        }
        if config.merge_threads.is_some() && n % config.merge_every == 0 {
            merge_step(&mut db);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The kill loop
// ---------------------------------------------------------------------------------------------------------------

/// What a kill loop varies.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct Scenario {
    collections: &'static [&'static str],
    /// Children pick a memtable threshold (the default or a few KiB) and may never commit; the parent reopens with
    /// either threshold and commits after a reopen only sometimes, so backlogs reach the next open.
    backlog: bool,
    /// Children may run merges.
    merges: bool,
    /// Some children are killed a seeded delay after `opening`.
    open_kills: bool,
}

#[cfg(unix)]
const COMMIT_PATH: Scenario = Scenario {
    collections: &["docs"],
    backlog: false,
    merges: false,
    open_kills: false,
};

#[cfg(unix)]
const RECOVERY: Scenario = Scenario {
    collections: &["a", "b"],
    backlog: true,
    merges: true,
    open_kills: true,
};

#[cfg(unix)]
impl Scenario {
    fn collections(&self) -> Vec<String> {
        self.collections.iter().map(|&c| c.to_owned()).collect()
    }

    /// A memtable threshold: the default, or a few KiB (the second scenario only).
    fn memtable_bytes(&self, rng: &mut Rng, default_percent: u64) -> usize {
        if !self.backlog || rng.chance(default_percent) {
            FlushPolicy::DEFAULT_MEMTABLE_BYTES
        } else {
            rng.range(4 << 10, 32 << 10) as usize
        }
    }

    /// The phase of the `kill`th child: with backlogs, a builder, then a flusher, then a mixed child, in turn.
    fn phase(&self, kill: u64) -> Phase {
        match kill % 3 {
            _ if !self.backlog => Phase::Mixed,
            0 => Phase::Build,
            1 => Phase::Flush,
            _ => Phase::Mixed,
        }
    }

    fn child_config(&self, rng: &mut Rng, phase: Phase) -> ChildConfig {
        let memtable_bytes = match phase {
            Phase::Build => FlushPolicy::DEFAULT_MEMTABLE_BYTES,
            Phase::Flush => self.memtable_bytes(rng, 0),
            Phase::Mixed => self.memtable_bytes(rng, 40),
        };
        let commit_every = match phase {
            Phase::Build => 0,
            _ if self.backlog && rng.chance(50) => 0,
            _ => rng.range(1, if self.backlog { 16 } else { 12 }),
        };
        let merge_threads = match rng.next_u64() % 3 {
            _ if !self.merges => None,
            0 => None,
            t => Some(t as usize - 1),
        };
        ChildConfig {
            collections: self.collections(),
            memtable_bytes,
            merge_threads,
            merge_every: rng.range(1, 6),
            commit_every,
        }
    }

    /// When to kill a child of `phase`.
    fn kill(&self, rng: &mut Rng, phase: Phase) -> Kill {
        let in_open = match phase {
            Phase::Build => 0,
            Phase::Flush => 75,
            Phase::Mixed => 20,
        };
        if self.open_kills && rng.chance(in_open) {
            Kill::InOpen(Duration::from_micros(rng.range(0, 50_000)))
        } else {
            let (lo, hi) = match phase {
                _ if !self.backlog => (1, 60),
                Phase::Build => (60, 160),
                _ => (1, 100),
            };
            Kill::AfterAcks(rng.range(lo, hi), Duration::from_micros(rng.range(0, 3000)))
        }
    }
}

/// What a child of the backlog scenario is for.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The default memtable threshold, never commits, killed after many operations: the backlog grows (merge
    /// commits flush nothing, so they keep it). The parent's reopens after it keep it too.
    Build,
    /// A threshold of a few KiB: its open's replay flushes the backlog, and it is often killed during the open.
    Flush,
    /// Anything.
    Mixed,
}

/// When the parent kills a child.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
enum Kill {
    /// After this many acknowledgements and the delay.
    AfterAcks(u64, Duration),
    /// The delay after `opening`: before, inside or (rarely) after the open.
    InOpen(Duration),
}

/// A read-only open the parent made while the child was alive.
#[cfg(unix)]
#[derive(Debug)]
struct LiveView {
    /// The child's acknowledgements before the reader opened.
    acked: usize,
    documents: BTreeMap<Key, Document>,
}

/// What one killed child reported.
#[cfg(unix)]
#[derive(Debug, Default)]
struct ChildRun {
    /// Whether it reported `opened`: the `seq` after the open, and its duration in microseconds.
    opened: Option<(u64, u64)>,
    /// The acknowledged operations, in order.
    acked: Vec<Op>,
    /// The operation it reported as started but not as acknowledged: it may or may not have taken effect.
    in_flight: Option<Op>,
    /// The last `seq` `commit()` or a merge commit returned.
    commit: Option<u64>,
    /// Merge commits.
    merges: u64,
    live_view: Option<LiveView>,
}

#[cfg(unix)]
fn spawn_writer(path: &Path, seed: u64, prefix: &str, known: &Path, config: &ChildConfig) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, "writer")
        .env(PATH_ENV, path)
        .env(SEED_ENV, seed.to_string())
        .env(PREFIX_ENV, prefix)
        .env(KNOWN_ENV, known)
        .env(CONFIG_ENV, config.encode())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

/// Writes `model` for the next child to pick keys from.
#[cfg(unix)]
fn write_known(path: &Path, model: &Model) {
    let mut out = String::new();
    for ((c, id), state) in model {
        let kind = if state.is_some() { "live" } else { "deleted" };
        for part in [kind, " ", c, " ", id, "\n"] {
            out.push_str(part);
        }
    }
    std::fs::write(path, out).unwrap();
}

#[cfg(unix)]
fn record(msg: &str, run: &mut ChildRun, ctx: &str) {
    let (verb, rest) = msg.split_once(' ').unwrap_or((msg, ""));
    match verb {
        "opening" => {}
        "opened" => {
            let (seq, micros) = rest.split_once(' ').unwrap();
            run.opened = Some((seq.parse().unwrap(), micros.parse().unwrap()));
        }
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
        "commit" => run.commit = run.commit.max(Some(rest.parse().unwrap())),
        "merge" => {
            run.merges += 1;
            run.commit = run.commit.max(Some(rest.parse().unwrap()));
        }
        other => panic!("{ctx}: unknown child report {other}"),
    }
}

/// The read-only checks beside a live writer (invariant 0): the open and `verify` succeed, a second writer is
/// locked out, and the documents are kept for [`check_live_view`].
#[cfg(unix)]
fn look_beside_writer(path: &Path, sc: &Scenario, acked: usize, ctx: &str) -> LiveView {
    let ro = ReadOnlyDatabase::open(path, &AnalyzerRegistry::builtin())
        .unwrap_or_else(|e| panic!("{ctx}: read-only open beside the writer failed: {e}"));
    ro.verify()
        .unwrap_or_else(|e| panic!("{ctx}: verify beside the writer failed: {e}"));
    let documents = documents(&ro.snapshot(), sc.collections);
    drop(ro);
    match Database::open(path, options(0, FlushPolicy::DEFAULT_MEMTABLE_BYTES, 0)) {
        Err(DatabaseError::Container(ContainerError::Locked)) => {}
        Err(e) => panic!("{ctx}: a second writer failed with {e}, not Locked"),
        Ok(_) => panic!("{ctx}: a second writer opened beside a live one"),
    }
    LiveView { acked, documents }
}

/// Runs one writer child on `path` (fresh ids `<prefix>-*`, existing keys picked from `known`) and SIGKILLs it as
/// `kill` says, looking at the file read-only at `look_at` acknowledgements.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn run_and_kill(
    path: &Path,
    known: &Path,
    sc: &Scenario,
    config: &ChildConfig,
    child_seed: u64,
    kill: Kill,
    look_at: u64,
    prefix: &str,
    ctx: &str,
) -> ChildRun {
    let mut child = spawn_writer(path, child_seed, prefix, known, config);
    let mut out = BufReader::new(child.stdout.take().unwrap());
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
    loop {
        let Ok(msg) = rx.recv() else {
            let status = child.wait().unwrap();
            panic!(
                "{ctx}: writer exited before the kill (opened: {:?}) after {} ops: {status}",
                run.opened,
                run.acked.len()
            );
        };
        record(&msg, &mut run, ctx);
        if let Kill::InOpen(delay) = kill
            && msg == "opening"
        {
            std::thread::sleep(delay);
            break;
        }
        if run.opened.is_some() && run.live_view.is_none() && run.acked.len() as u64 >= look_at {
            run.live_view = Some(look_beside_writer(path, sc, run.acked.len(), ctx));
        }
        if let Kill::AfterAcks(count, delay) = kill
            && run.acked.len() as u64 >= count
        {
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
        record(&msg, &mut run, ctx);
    }
    assert!(
        !status.success(),
        "{ctx}: writer exited by itself: {status}"
    );
    run
}

/// The document a model state stands for: `None` for deleted or never written.
fn expected(key: &Key, state: Option<&Option<String>>) -> Option<Document> {
    state
        .and_then(|s| s.as_deref())
        .map(|token| doc(&key.1, token))
}

/// The first few keys where `a` and `b` differ, for failure messages.
#[cfg(unix)]
fn diff(a: &BTreeMap<Key, Document>, b: &BTreeMap<Key, Document>) -> String {
    let keys: BTreeSet<&Key> = a.keys().chain(b.keys()).collect();
    let differ: Vec<String> = keys
        .into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| format!("{k:?}: {} vs {}", describe(a.get(k)), describe(b.get(k))))
        .collect();
    format!(
        "{} keys differ: {}",
        differ.len(),
        differ
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ")
    )
}

fn describe(d: Option<&Document>) -> String {
    match d {
        None => "absent".to_owned(),
        Some(d) => format!("{d:?}").chars().take(200).collect(),
    }
}

/// Checks invariant 3 on every collection of `snapshot`: no id is live in two segments, and the manifest's segment
/// entries are the snapshot's segments with their tombstone counts.
#[cfg(unix)]
fn check_segments(snapshot: &Snapshot, collections: &[&str], ctx: &str) {
    for &name in collections {
        let ctx = format!("{ctx}, collection {name}");
        let c = snapshot.collection(name).unwrap();
        let entries = snapshot
            .manifest()
            .collection(name)
            .unwrap_or_else(|| panic!("{ctx}: no manifest entry"))
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
}

/// Checks invariants 1-4 on `snapshot` (committed at `seq`) against `model` (every acknowledged operation applied),
/// `in_flight` (the killed child's unacknowledged operation, if any) and `last_commit`. Returns the state the
/// in-flight operation's key has in the snapshot, to update the model with (`None` if it is absent and not in the
/// model).
#[cfg(unix)]
fn check(
    snapshot: &Snapshot,
    seq: u64,
    collections: &[&str],
    model: &Model,
    in_flight: Option<&Op>,
    last_commit: Option<u64>,
    ctx: &str,
) -> Option<(Key, Option<String>)> {
    for (key, state) in model {
        let actual = get(snapshot, &key.0, &key.1);
        let want = expected(key, Some(state));
        let by_in_flight = in_flight
            .filter(|op| op.key() == *key)
            .is_some_and(|op| actual == expected(key, Some(&op.effect())));
        assert!(
            actual == want || by_in_flight,
            "{ctx}: {key:?} should be {}, but is {}",
            describe(want.as_ref()),
            describe(actual.as_ref())
        );
    }
    let resolved = in_flight.and_then(|op| {
        let key = op.key();
        let actual = get(snapshot, &key.0, &key.1);
        let before = model.get(&key);
        if actual == expected(&key, Some(&op.effect())) {
            Some((key, op.effect()))
        } else {
            assert!(
                actual == expected(&key, before),
                "{ctx}: in-flight {op:?} left {}",
                describe(actual.as_ref())
            );
            before.map(|s| (key, s.clone()))
        }
    });
    for key in documents(snapshot, collections).into_keys() {
        let live_in_model = matches!(model.get(&key), Some(Some(_)));
        let in_flight_put = matches!(in_flight, Some(op @ Op::Put { .. }) if op.key() == key);
        assert!(
            live_in_model || in_flight_put,
            "{ctx}: {key:?} is visible but {}",
            if model.contains_key(&key) {
                "deleted"
            } else {
                "never written (phantom)"
            }
        );
    }
    check_segments(snapshot, collections, ctx);
    if let Some(last) = last_commit {
        assert!(
            seq >= last,
            "{ctx}: seq {seq} is below the acknowledged commit {last}"
        );
    }
    resolved
}

/// Checks what a reader beside the live child saw (invariant 0): for each key, the state after `model` and the
/// operations acknowledged before the reader opened, or the effect of a later operation of the child.
#[cfg(unix)]
fn check_live_view(model: &Model, run: &ChildRun, view: &LiveView, ctx: &str) {
    let mut base = model.clone();
    for op in &run.acked[..view.acked] {
        base.insert(op.key(), op.effect());
    }
    let later: Vec<&Op> = run.acked[view.acked..]
        .iter()
        .chain(run.in_flight.as_ref())
        .collect();
    let keys: BTreeSet<&Key> = base.keys().chain(view.documents.keys()).collect();
    for key in keys {
        let actual = view.documents.get(key);
        let allowed = actual == expected(key, base.get(key)).as_ref()
            || later
                .iter()
                .any(|op| op.key() == *key && actual == expected(key, Some(&op.effect())).as_ref());
        assert!(
            allowed,
            "{ctx}: the reader beside the writer (after {} acks) shows {key:?} as {}, but it should be {}",
            view.acked,
            describe(actual),
            describe(expected(key, base.get(key)).as_ref())
        );
    }
}

/// Counts over the run.
#[cfg(unix)]
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    inserts: u64,
    upserts: u64,
    deletes: u64,
    /// Upserts and deletes of keys an earlier child wrote: committed, or replayed from the WAL on open.
    of_earlier: u64,
    /// Live documents after the last kill.
    docs: usize,
    /// Segments with a tombstone sidecar after the last kill.
    sidecars: usize,
    /// Kills before the child reported `opened`.
    kills_in_open: u64,
    /// Of those, the ones after the open had committed (a replay-time flush, ROADMAP 6.6.5).
    kills_after_replay_commit: u64,
    /// Merge commits children reported.
    merges: u64,
    /// Read-write opens (children's and the parent's) whose replay committed.
    replay_commits: u64,
    /// The largest backlog (WAL records replayed) a read-only open found.
    max_backlog: u64,
    /// The slowest open a child reported, in microseconds.
    max_open_micros: u64,
}

#[cfg(unix)]
impl Stats {
    fn add(&mut self, s: &Self) {
        self.inserts += s.inserts;
        self.upserts += s.upserts;
        self.deletes += s.deletes;
        self.of_earlier += s.of_earlier;
        self.docs += s.docs;
        self.sidecars += s.sidecars;
        self.kills_in_open += s.kills_in_open;
        self.kills_after_replay_commit += s.kills_after_replay_commit;
        self.merges += s.merges;
        self.replay_commits += s.replay_commits;
        self.max_backlog = self.max_backlog.max(s.max_backlog);
        self.max_open_micros = self.max_open_micros.max(s.max_open_micros);
    }
}

/// A read-only open and `verify` of `path` (invariant 0): its documents, `seq`, and the records it replayed.
#[cfg(unix)]
fn read_only(path: &Path, sc: &Scenario, ctx: &str) -> (BTreeMap<Key, Document>, u64, u64) {
    let ro = ReadOnlyDatabase::open(path, &AnalyzerRegistry::builtin())
        .unwrap_or_else(|e| panic!("{ctx}: read-only open failed: {e}"));
    let report = ro
        .verify()
        .unwrap_or_else(|e| panic!("{ctx}: verify failed: {e}"));
    assert_eq!(report.seq, ro.seq(), "{ctx}");
    assert_eq!(report.collections, sc.collections.len(), "{ctx}");
    (
        documents(&ro.snapshot(), sc.collections),
        ro.seq(),
        report.replayed,
    )
}

/// The temp dir of one file's loop, kept on a panic (with `after-kill.persia`, the file as the last kill left it) so
/// a failure can be replayed from its bytes.
#[cfg(unix)]
struct Artifacts(Option<tempfile::TempDir>);

#[cfg(unix)]
impl Artifacts {
    fn path(&self) -> &Path {
        self.0.as_ref().unwrap().path()
    }
}

#[cfg(unix)]
impl Drop for Artifacts {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(dir) = self.0.take()
        {
            eprintln!("artifacts kept in {}", dir.keep().display());
        }
    }
}

/// The parent's state over one file's kill loop.
#[cfg(unix)]
struct FileLoop {
    sc: Scenario,
    path: PathBuf,
    /// The file as the last kill left it, kept with the [`Artifacts`] on a failure.
    after_kill: PathBuf,
    /// Every acknowledged operation applied, and each in-flight one as the reopen resolved it.
    model: Model,
    last_commit: Option<u64>,
    /// The `seq` before the next child: an open that reports a later one committed during replay.
    seq_before: u64,
    stats: Stats,
}

#[cfg(unix)]
impl FileLoop {
    /// Applies the acknowledged operations of `run` to the model; returns the model before them.
    fn apply(&mut self, run: &ChildRun) -> Model {
        let before = self.model.clone();
        for op in &run.acked {
            match op {
                Op::Put { .. } if self.model.get(&op.key()).is_some_and(Option::is_some) => {
                    self.stats.upserts += 1;
                }
                Op::Put { .. } => self.stats.inserts += 1,
                Op::Delete { .. } => self.stats.deletes += 1,
            }
            if before.contains_key(&op.key()) {
                self.stats.of_earlier += 1;
            }
            self.model.insert(op.key(), op.effect());
        }
        self.last_commit = self.last_commit.max(run.commit);
        self.stats.merges += run.merges;
        before
    }

    /// Invariant 0 after the kill, before any read-write open: returns the read-only documents and `seq`.
    fn read_only_after_kill(
        &mut self,
        run: &ChildRun,
        ctx: &str,
    ) -> (BTreeMap<Key, Document>, u64) {
        std::fs::copy(&self.path, &self.after_kill).unwrap();
        let (docs, seq, _) = read_only(
            &self.path,
            &self.sc,
            &format!("{ctx}, read-only after kill"),
        );
        if let Some((opened_seq, micros)) = run.opened {
            self.stats.max_open_micros = self.stats.max_open_micros.max(micros);
            if opened_seq > self.seq_before {
                self.stats.replay_commits += 1;
            }
        } else {
            self.stats.kills_in_open += 1;
            if seq > self.seq_before {
                self.stats.kills_after_replay_commit += 1;
            }
        }
        if let Some(last) = self.last_commit {
            assert!(seq >= last, "{ctx}: read-only seq {seq} < {last}");
        }
        (docs, seq)
    }

    /// A read-write open with a memtable threshold of `bytes`.
    fn open(&mut self, rng: &mut Rng, bytes: usize, ctx: &str) -> Database {
        Database::open(&self.path, options(rng.next_u64(), bytes, 0))
            .unwrap_or_else(|e| panic!("{ctx}: open failed: {e}"))
    }

    /// Invariants 0-5 after the killed child `run` of `phase`.
    fn check_after_kill(&mut self, rng: &mut Rng, run: &ChildRun, phase: Phase, ctx: &str) {
        let sc = self.sc;
        let model_before = self.apply(run);
        let (ro_docs, ro_seq) = self.read_only_after_kill(run, ctx);

        // After a builder, the parent keeps the backlog for the flusher: the default threshold and no commit.
        let keep = phase == Phase::Build;
        let bytes = |rng: &mut Rng| {
            if keep {
                FlushPolicy::DEFAULT_MEMTABLE_BYTES
            } else {
                sc.memtable_bytes(rng, 75)
            }
        };
        let first_bytes = bytes(rng);
        let reopen_ctx = format!(
            "{ctx}, reopen (memtable {first_bytes}, in flight: {:?})",
            run.in_flight
        );
        let mut db = self.open(rng, first_bytes, &reopen_ctx);
        if db.seq() > ro_seq {
            self.stats.replay_commits += 1;
        }
        let snapshot = db.snapshot();
        let resolved = check(
            &snapshot,
            db.seq(),
            sc.collections,
            &self.model,
            run.in_flight.as_ref(),
            self.last_commit,
            &reopen_ctx,
        );
        if let Some((key, state)) = resolved {
            self.model.insert(key, state);
        }
        let present = documents(&snapshot, sc.collections);
        assert!(
            present == ro_docs,
            "{reopen_ctx}: the read-only open showed other documents than the read-write reopen (rw vs ro): {}",
            diff(&present, &ro_docs)
        );
        if !sc.backlog || (!keep && rng.chance(33)) {
            let seq = db
                .commit()
                .unwrap_or_else(|e| panic!("{ctx}: commit after reopen failed: {e}"));
            self.last_commit = self.last_commit.max(Some(seq));
            let commit_ctx = format!("{ctx}, commit");
            let snapshot = db.snapshot();
            assert!(
                documents(&snapshot, sc.collections) == present,
                "{commit_ctx}: changed documents"
            );
            check(
                &snapshot,
                db.seq(),
                sc.collections,
                &self.model,
                None,
                self.last_commit,
                &commit_ctx,
            );
        }
        let seq = db.seq();
        drop(db);
        let second_bytes = bytes(rng);
        self.check_second_reopen(rng, second_bytes, seq, &present, ctx);
        if let Some(view) = &run.live_view {
            check_live_view(&model_before, run, view, ctx);
        }
    }

    /// Invariant 5: a second read-write open (threshold `second_bytes`) after the first left `seq` shows `present`,
    /// and so does a read-only open beside it.
    fn check_second_reopen(
        &mut self,
        rng: &mut Rng,
        second_bytes: usize,
        seq: u64,
        present: &BTreeMap<Key, Document>,
        ctx: &str,
    ) {
        let sc = self.sc;
        let second_ctx = format!("{ctx}, second reopen (memtable {second_bytes})");
        let db = self.open(rng, second_bytes, &second_ctx);
        if db.seq() > seq {
            self.stats.replay_commits += 1;
        }
        if std::env::var_os("PERSIA_TEST_VERBOSE").is_some() {
            eprintln!("{second_ctx}: seq {}", db.seq());
        }
        let snapshot = db.snapshot();
        check(
            &snapshot,
            db.seq(),
            sc.collections,
            &self.model,
            None,
            self.last_commit,
            &second_ctx,
        );
        assert!(
            documents(&snapshot, sc.collections) == *present,
            "{second_ctx}: changed documents"
        );
        // Beside the open writer; the backlog it reports is what the next child replays.
        let (beside, _, backlog) =
            read_only(&self.path, &sc, &format!("{second_ctx}, read-only beside"));
        assert!(
            beside == *present,
            "{second_ctx}: read-only beside differs (rw vs ro): {}",
            diff(present, &beside)
        );
        self.seq_before = db.seq();
        self.stats.max_backlog = self.stats.max_backlog.max(backlog);
        self.stats.docs = present.len();
        self.stats.sidecars = sc
            .collections
            .iter()
            .map(|c| {
                snapshot
                    .collection(c)
                    .unwrap()
                    .tombstones()
                    .iter()
                    .flatten()
                    .count()
            })
            .sum();
    }
}

/// `kills` killed writers on one new database.
#[cfg(unix)]
fn kill_loop_on_one_file(rng: &mut Rng, sc: &Scenario, kills: u64, ctx: &str) -> Stats {
    let dir = Artifacts(Some(tempfile::tempdir().unwrap()));
    let path = dir.path().join("app.persia");
    let known = dir.path().join("known.txt");
    let seq_before = {
        let options = options(rng.next_u64(), FlushPolicy::DEFAULT_MEMTABLE_BYTES, 0);
        let mut db = Database::create(&path, options).unwrap();
        for c in sc.collections {
            db.create_collection(schema(c)).unwrap();
        }
        db.seq()
    };
    let mut state = FileLoop {
        sc: *sc,
        after_kill: dir.path().join("after-kill.persia"),
        path,
        model: Model::new(),
        last_commit: None,
        seq_before,
        stats: Stats::default(),
    };
    for kill in 0..kills {
        let prefix = format!("k{kill}");
        write_known(&known, &state.model);
        let phase = sc.phase(kill);
        let config = sc.child_config(rng, phase);
        let child_seed = rng.next_u64();
        let kill_at = sc.kill(rng, phase);
        let look_at = rng.range(0, 60);
        let ctx =
            format!("{ctx}, kill {kill} ({phase:?}, {config:?}, {kill_at:?}, look at {look_at})");
        let run = run_and_kill(
            &state.path,
            &known,
            sc,
            &config,
            child_seed,
            kill_at,
            look_at,
            &prefix,
            &ctx,
        );
        state.check_after_kill(rng, &run, phase, &ctx);
    }
    state.stats
}

#[cfg(unix)]
fn kill_loop(sc: &Scenario, kills: u64, name: &str) {
    let seed = base_seed();
    let files = env_count("PERSIA_TEST_FILES", FILES);
    let kills = env_count("PERSIA_TEST_KILLS", kills);
    let mut rng = Rng(seed);
    let started = std::time::Instant::now();
    let mut total = Stats::default();
    for file in 0..files {
        let s = kill_loop_on_one_file(&mut rng, sc, kills, &format!("seed {seed}, file {file}"));
        total.add(&s);
    }
    eprintln!(
        "{name}: {} kills on {files} files ({} before `opened`, {} of them after a replay-time commit); \
         acknowledged {} inserts, {} upserts, {} deletes ({} on keys of earlier children); {} merge commits; \
         {} opens committed during replay; largest backlog {} records; slowest child open {} us; \
         {} documents and {} tombstone sidecars at the end; {:?}",
        files * kills,
        total.kills_in_open,
        total.kills_after_replay_commit,
        total.inserts,
        total.upserts,
        total.deletes,
        total.of_earlier,
        total.merges,
        total.replay_commits,
        total.max_backlog,
        total.max_open_micros,
        total.docs,
        total.sidecars,
        started.elapsed()
    );
}

/// Writer children put, upsert and delete with `Fsync` and commit on a real file, also over ids earlier children
/// committed, and are killed with SIGKILL at seeded points; every acknowledged operation survives every reopen, no
/// deleted id comes back, and nothing unacknowledged but an in-flight operation shows.
#[cfg(unix)]
#[test]
fn kill_9_loop_keeps_every_acknowledged_write() {
    kill_loop(&COMMIT_PATH, KILLS_PER_FILE, "database kill-9 loop");
}

/// As [`kill_9_loop_keeps_every_acknowledged_write`] over two collections, with backlogs that the next open's replay
/// flushes, merges, and kills during the open (ROADMAP 6.6.4, 6.6.5; SPEC §8.6): the same invariants, plus a
/// read-only open and `verify` after every kill and beside every live writer.
#[cfg(unix)]
#[test]
fn kill_9_loop_through_replay_flushes_merges_and_open() {
    kill_loop(
        &RECOVERY,
        RECOVERY_KILLS_PER_FILE,
        "database recovery kill-9 loop",
    );
}

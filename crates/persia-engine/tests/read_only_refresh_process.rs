//! Real-file, real-process test of refreshing one [`ReadOnlyDatabase`] in place while writers come and go (ROADMAP
//! 6.6.6; SPEC §8.6; ADR-0060).
//!
//! The parent creates a database in a temp-dir container file, opens ONE `ReadOnlyDatabase` on it, and keeps it for
//! the whole run. Two threads use it meanwhile: a refresher calls [`ReadOnlyDatabase::refresh`] in a loop, and a
//! viewer takes snapshots and checks each of them, with `get`s and term searches. A writer child (this test binary
//! re-executed with [`ROLE_ENV`] set, as in `database_process.rs`) opens the file, commits, reports `ready`, waits for
//! the parent's `go`, then runs a seeded workload: groups (a `put_batch` of 1 to 6 documents sharing one body token,
//! `Fsync` or `Async`, so one WAL chunk), upserts of whole groups, single puts, upserts and deletes of "solo" ids,
//! commits every few operations, inline merges (`MergeOptions { threads: 0 }`, `segments_per_merge: 2`), and now and
//! then a created collection or a compaction (`Database::compact`, which renames a new file over the path). The parent
//! SIGKILLs it at a seeded point (after some acknowledgements, a seeded delay after a commit, merge, compaction or
//! collection creation began, or inside the open), and starts the next writer on the same file. The last writer
//! stops by itself after a seeded number of operations.
//!
//! Invariants:
//! 1. Every refresh succeeds (no error at all, so neither `is_corruption()` nor `RefreshRefused`): the file is only
//!    ever written by one live or killed writer of this database.
//! 2. The reader keeps serving: every snapshot the viewer takes answers `get` and searches without error, and is
//!    internally consistent under one memtable view: a group is visible with all its members or none, all with the
//!    same body token (one WAL chunk, applied under the memtable hold); every document is the one its id and token
//!    stand for; a term search for a visible group's token finds exactly the documents showing that token.
//! 3. `seq` never goes backwards: across the successful refreshes of each thread, and across the viewer's snapshots.
//! 4. After every kill (the file is still), one refresh shows every operation the killed writer acknowledged
//!    (`Fsync` and `Async`: SIGKILL kills the process, not the machine), every earlier one, and the in-flight
//!    operation either whole or not at all; no deleted id and no phantom. It equals a fresh `ReadOnlyDatabase::open`
//!    (seq, manifest, `replayed()`, `wal_end()`, segments, documents), and its `seq` is at least the last commit the
//!    writer reported.
//! 5. After the next writer's first commit (it reported `ready`), one refresh shows the same documents again.
//! 6. After the last writer exits cleanly, one refresh makes the reader equal a fresh open (as in 4), shows exactly
//!    the model, and `verify()` passes on both.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed, and every failure message names it; a run is
//! replayed with `PERSIA_TEST_SEED=<seed> cargo test -p persia-engine --test read_only_refresh_process`. Kill instants
//! also depend on timing, so a replay reproduces the schedule, not the exact interleaving. `PERSIA_TEST_FILES` and
//! `PERSIA_TEST_ROUNDS` override the number of files and writers per file. On a failure the file's temp dir is kept
//! (its path is printed), with `after-kill.persia`, the file as the last kill left it.
//!
//! The children run this test binary from its path: rebuilding it while the test runs makes later children run other
//! code than the parent.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{
    CollectionSnapshot, Database, DatabaseError, DatabaseOptions, Located, ManualClock,
    MergeOutcome, ReadOnlyDatabase, Refreshed, SegmentIds, Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::memtable::MemtableView;
use persia_engine::merge::{MergeOptions, MergePolicy};
use persia_engine::query::{Query, TermQuery};
use persia_engine::schema::{CollectionSchema, FieldId, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::{Pruning, SearchPlan};
use persia_engine::view::SearchView;
use persia_engine::wal::Durability;
use persia_storage::container::Error as ContainerError;

/// Selects the child role of a re-executed test binary.
const ROLE_ENV: &str = "PERSIA_REFRESH_CHILD_ROLE";
/// The database path a child works on.
const PATH_ENV: &str = "PERSIA_REFRESH_CHILD_PATH";
/// The seed of a child's schedule.
const SEED_ENV: &str = "PERSIA_REFRESH_CHILD_SEED";
/// The prefix of a child's fresh ids, tokens and collection names: unique per child on a file.
const PREFIX_ENV: &str = "PERSIA_REFRESH_CHILD_PREFIX";
/// The child's [`ChildConfig`], encoded.
const CONFIG_ENV: &str = "PERSIA_REFRESH_CHILD_CONFIG";
/// Prefix of the lines children report; anything else on their stdout is libtest's.
const TAG: &str = "@@persia ";

/// The collection the parent creates.
const FIRST_COLLECTION: &str = "a";
/// Children create collections up to this many.
const MAX_COLLECTIONS: usize = 4;

#[cfg(all(unix, not(feature = "slow")))]
const FILES: u64 = 1;
#[cfg(all(unix, feature = "slow"))]
const FILES: u64 = 4;

/// Writers per file, the last one exiting cleanly.
#[cfg(all(unix, not(feature = "slow")))]
const ROUNDS: u64 = 6;
#[cfg(all(unix, feature = "slow"))]
const ROUNDS: u64 = 30;

fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_0666_7e00);
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
    fn pick<'a, T>(&mut self, set: &'a BTreeSet<T>) -> &'a T {
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

/// The `body` field: every collection has the same schema, so the same id.
fn body_field() -> FieldId {
    schema("x").field("body").unwrap().id()
}

/// The body a put of `id` with body token `token` writes: `<id> <token>` and seeded filler words, a pure function of
/// both, so any document can be checked against its id and token. Lengths are skewed short with an occasional
/// multi-KB body (a group stays far below the 1 MiB WAL chunk target, so its batch is one chunk).
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
        0 => rng.range(200, 800),
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
    Document::new(id).with(body_field(), Value::String(body_of(id, token)))
}

/// The body token a document shows (its second word), if its body is a string.
fn token_of(d: &Document) -> Option<&str> {
    match d.get(body_field()) {
        Some(Value::String(s)) => s.split(' ').nth(1),
        _ => None,
    }
}

/// The id of member `i` of group `n` (of `size` members) of the child with prefix `prefix`.
fn group_member(prefix: &str, n: u64, size: u64, i: u64) -> String {
    format!("{prefix}-g{n}-x{size}-m{i}")
}

/// The group of a member id: `(prefix, n, size)`, or `None` for a solo id.
fn group_of(id: &str) -> Option<(String, u64, u64)> {
    let parts: Vec<&str> = id.split('-').collect();
    match parts.as_slice() {
        [prefix, g, x, m] if g.starts_with('g') && x.starts_with('x') && m.starts_with('m') => {
            Some((
                (*prefix).to_owned(),
                g.strip_prefix('g')?.parse().ok()?,
                x.strip_prefix('x')?.parse().ok()?,
            ))
        }
        _ => None,
    }
}

/// The id of the `n`th solo put of the child with prefix `prefix`.
fn solo_id(prefix: &str, n: u64) -> String {
    format!("{prefix}-s{n}")
}

/// Options of every writer: a fixed clock (only explicit commits), seeded segment ids, inline merges of two tier-0
/// segments, the default memtable threshold (no flush inside a batch) and WAL chunk target.
fn options(seed: u64) -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)),
        segment_ids: SegmentIds::seeded(seed),
        merge: MergeOptions {
            policy: MergePolicy {
                segments_per_merge: 2,
                ..MergePolicy::default()
            },
            threads: 0,
        },
        ..DatabaseOptions::default()
    }
}

/// A (collection, id) pair.
type Key = (String, String);

/// The parent's model: key -> the body token of its live version, or `None` once deleted.
type Model = BTreeMap<Key, Option<String>>;

fn durability_name(d: Durability) -> &'static str {
    match d {
        Durability::Fsync => "fsync",
        Durability::Async => "async",
        Durability::Buffered => "buffered",
    }
}

fn durability_of(s: &str) -> Durability {
    match s {
        "fsync" => Durability::Fsync,
        "async" => Durability::Async,
        other => panic!("bad durability {other:?}"),
    }
}

/// One document write of the workload.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    /// A `put_batch` of every member of group `(prefix, n, size)` with body token `token`: a new group, or an upsert
    /// of all of an existing one.
    Group {
        collection: String,
        prefix: String,
        n: u64,
        size: u64,
        token: String,
        durability: Durability,
    },
    /// A `put` (insert, upsert or resurrection) of solo id `id`.
    Put {
        collection: String,
        id: String,
        token: String,
        durability: Durability,
    },
    /// A `delete` of solo id `id`.
    Delete {
        collection: String,
        id: String,
        durability: Durability,
    },
}

impl Op {
    /// The model state of each key it writes, after it.
    fn effects(&self) -> Vec<(Key, Option<String>)> {
        match self {
            Self::Group {
                collection,
                prefix,
                n,
                size,
                token,
                ..
            } => (0..*size)
                .map(|i| {
                    (
                        (collection.clone(), group_member(prefix, *n, *size, i)),
                        Some(token.clone()),
                    )
                })
                .collect(),
            Self::Put {
                collection,
                id,
                token,
                ..
            } => vec![((collection.clone(), id.clone()), Some(token.clone()))],
            Self::Delete { collection, id, .. } => vec![((collection.clone(), id.clone()), None)],
        }
    }

    fn durability(&self) -> Durability {
        match self {
            Self::Group { durability, .. }
            | Self::Put { durability, .. }
            | Self::Delete { durability, .. } => *durability,
        }
    }

    /// Space-separated; none of the parts has spaces.
    fn encode(&self) -> String {
        match self {
            Self::Group {
                collection,
                prefix,
                n,
                size,
                token,
                durability,
            } => format!(
                "group {collection} {prefix} {n} {size} {token} {}",
                durability_name(*durability)
            ),
            Self::Put {
                collection,
                id,
                token,
                durability,
            } => format!(
                "put {collection} {id} {token} {}",
                durability_name(*durability)
            ),
            Self::Delete {
                collection,
                id,
                durability,
            } => format!("delete {collection} {id} {}", durability_name(*durability)),
        }
    }

    fn decode(s: &str) -> Self {
        let parts: Vec<&str> = s.split(' ').collect();
        match parts.as_slice() {
            ["group", c, prefix, n, size, token, d] => Self::Group {
                collection: (*c).to_owned(),
                prefix: (*prefix).to_owned(),
                n: n.parse().unwrap(),
                size: size.parse().unwrap(),
                token: (*token).to_owned(),
                durability: durability_of(d),
            },
            ["put", c, id, token, d] => Self::Put {
                collection: (*c).to_owned(),
                id: (*id).to_owned(),
                token: (*token).to_owned(),
                durability: durability_of(d),
            },
            ["delete", c, id, d] => Self::Delete {
                collection: (*c).to_owned(),
                id: (*id).to_owned(),
                durability: durability_of(d),
            },
            _ => panic!("bad op {s:?}"),
        }
    }
}

/// What a child does, chosen by the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChildConfig {
    /// `commit()` after every this many operations; 0 for never.
    commit_every: u64,
    /// The chance, in percent, of a `merge_step` after a commit.
    merge_percent: u64,
    /// The chance, in percent, of an operation being a compaction.
    compact_percent: u64,
    /// The chance, in percent, of an operation being a created collection.
    create_percent: u64,
    /// The chance, in percent, of a write being `Async` rather than `Fsync`.
    async_percent: u64,
    /// Stop cleanly after this many document writes; 0 for never (killed).
    limit: u64,
}

impl ChildConfig {
    fn encode(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.commit_every,
            self.merge_percent,
            self.compact_percent,
            self.create_percent,
            self.async_percent,
            self.limit
        )
    }

    fn decode(s: &str) -> Self {
        let n: Vec<u64> = s.split(' ').map(|p| p.parse().unwrap()).collect();
        let [
            commit_every,
            merge_percent,
            compact_percent,
            create_percent,
            async_percent,
            limit,
        ] = n.as_slice()
        else {
            panic!("bad child config {s:?}");
        };
        Self {
            commit_every: *commit_every,
            merge_percent: *merge_percent,
            compact_percent: *compact_percent,
            create_percent: *create_percent,
            async_percent: *async_percent,
            limit: *limit,
        }
    }
}

/// Every live document of `c` as of `memtable`, ONE view of its memtable: a refresh meanwhile cannot show part of
/// itself, and a check of the documents and a search on the same view agree. The memtable's version or delete of an
/// id hides the segments' (SPEC §8.3).
fn view_documents(
    c: &CollectionSnapshot,
    memtable: &MemtableView,
) -> Result<BTreeMap<String, Document>, String> {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for docid in 0..memtable.doc_count() {
        if memtable.is_live(docid) {
            let d = memtable
                .document(docid)
                .map_err(|e| format!("memtable document {docid}: {e}"))?;
            ids.insert(d.id().to_owned());
        }
    }
    for (i, s) in c.segments().iter().enumerate() {
        for docid in 0..s.doc_count() {
            ids.insert(
                s.id_of(docid)
                    .map_err(|e| format!("segment {i} id_of {docid}: {e}"))?,
            );
        }
    }
    let mut out = BTreeMap::new();
    for id in ids {
        let located = c
            .lookup(memtable, &id)
            .map_err(|e| format!("lookup {id}: {e}"))?;
        let d = match located {
            None => continue,
            Some(Located::Memtable(docid)) => memtable
                .document(docid)
                .map_err(|e| format!("memtable document {docid} ({id}): {e}"))?,
            Some(Located::Segment { index, docid }) => c.segments()[index]
                .document(docid)
                .map_err(|e| format!("segment {index} document {docid} ({id}): {e}"))?,
        };
        out.insert(id, d);
    }
    Ok(out)
}

/// Every live document of every collection of `snapshot`, by key.
fn documents(snapshot: &Snapshot) -> BTreeMap<Key, Document> {
    let mut out = BTreeMap::new();
    for (name, c) in snapshot.collections() {
        for (id, d) in
            view_documents(c, &c.memtable()).unwrap_or_else(|e| panic!("collection {name}: {e}"))
        {
            out.insert((name.to_owned(), id), d);
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
    let config = ChildConfig::decode(&std::env::var(CONFIG_ENV).unwrap());
    match role.as_str() {
        "writer" => child_writer(&path, seed, &prefix, config),
        other => panic!("unknown child role {other}"),
    }
}

/// The parent's control channel on stdin: the first line is `go`; EOF means the parent is gone (it holds the
/// child's stdin until it reaped it), so the writer stops instead of running on.
fn control() -> (Receiver<()>, Arc<AtomicBool>) {
    let gone = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&gone);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = std::io::stdin().lock().lines();
        if let Some(Ok(line)) = lines.next() {
            assert_eq!(line, "go");
            tx.send(()).unwrap();
        }
        for line in lines {
            if line.is_err() {
                break;
            }
        }
        flag.store(true, Ordering::Relaxed);
    });
    (rx, gone)
}

/// What a writer knows of the database: the collections, the live groups, the solo ids.
#[derive(Debug, Default)]
struct Known {
    collections: Vec<String>,
    /// `(collection, prefix, n, size)`.
    groups: BTreeSet<(String, String, u64, u64)>,
    live: BTreeSet<Key>,
    deleted: BTreeSet<Key>,
}

impl Known {
    /// What `snapshot` shows (deleted solo ids of earlier writers are not known).
    fn of(snapshot: &Snapshot) -> Self {
        let mut known = Self {
            collections: snapshot.collections().map(|(n, _)| n.to_owned()).collect(),
            ..Self::default()
        };
        for (c, id) in documents(snapshot).into_keys() {
            match group_of(&id) {
                Some((prefix, n, size)) => {
                    known.groups.insert((c, prefix, n, size));
                }
                None => {
                    known.live.insert((c, id));
                }
            }
        }
        known
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Group {
                collection,
                prefix,
                n,
                size,
                ..
            } => {
                self.groups
                    .insert((collection.clone(), prefix.clone(), *n, *size));
            }
            Op::Put { collection, id, .. } => {
                let key = (collection.clone(), id.clone());
                self.deleted.remove(&key);
                self.live.insert(key);
            }
            Op::Delete { collection, id, .. } => {
                let key = (collection.clone(), id.clone());
                self.live.remove(&key);
                self.deleted.insert(key);
            }
        }
    }
}

/// The next document write: a new group (40%), an upsert of a whole live group (15%), a new solo id (17%), a delete
/// (13%), upsert (7%) or resurrection (8%) of a solo id; a new group when the pick has no candidate.
fn next_op(
    rng: &mut Rng,
    known: &Known,
    prefix: &str,
    fresh: &mut u64,
    token: String,
    durability: Durability,
) -> Op {
    let collection =
        known.collections[(rng.next_u64() % known.collections.len() as u64) as usize].clone();
    let op = match rng.next_u64() % 100 {
        40..55 if !known.groups.is_empty() => {
            let (collection, prefix, n, size) = rng.pick(&known.groups).clone();
            Some(Op::Group {
                collection,
                prefix,
                n,
                size,
                token: token.clone(),
                durability,
            })
        }
        55..72 => {
            *fresh += 1;
            Some(Op::Put {
                collection: collection.clone(),
                id: solo_id(prefix, *fresh),
                token: token.clone(),
                durability,
            })
        }
        72..85 if !known.live.is_empty() => {
            let (collection, id) = rng.pick(&known.live).clone();
            Some(Op::Delete {
                collection,
                id,
                durability,
            })
        }
        85..92 if !known.live.is_empty() => {
            let (collection, id) = rng.pick(&known.live).clone();
            Some(Op::Put {
                collection,
                id,
                token: token.clone(),
                durability,
            })
        }
        92..100 if !known.deleted.is_empty() => {
            let (collection, id) = rng.pick(&known.deleted).clone();
            Some(Op::Put {
                collection,
                id,
                token: token.clone(),
                durability,
            })
        }
        _ => None,
    };
    op.unwrap_or_else(|| {
        *fresh += 1;
        Op::Group {
            collection,
            prefix: prefix.to_owned(),
            n: *fresh,
            size: rng.range(1, 7),
            token,
            durability,
        }
    })
}

/// Commits, reporting `begin commit` before and `commit <seq>` after.
fn commit(db: &mut Database) {
    report("begin commit");
    let seq = db.commit().unwrap();
    report(&format!("commit {seq}"));
}

/// Runs `merge_step` (inline: one merge to its end), reporting `begin merge`, each merge commit as `merge <seq>`, and
/// `merged`. A failed merge means a damaged segment: the child panics, and the parent sees it exit before the kill.
fn merge_step(db: &mut Database) {
    report("begin merge");
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
    report("merged");
}

/// Opens the database at `path`, commits, reports `ready`, waits for `go`, then runs the workload of [`next_op`] with
/// commits, merges, compactions and created collections as `config` says, until killed, its stdin closes, or
/// `config.limit` writes (then it reports `done` and returns).
///
/// Reports `opening` before the open and `opened <seq>` after it; `op <n> <op>` before its `n`th write and `acked
/// <n>` once it returned; `begin <what>` before a commit, merge step, compaction or collection creation, and `commit
/// <seq>`, `merge <seq>` (per merge commit) and `merged`, `compacted <seq>`, `created <name> <seq>` after it.
fn child_writer(path: &Path, seed: u64, prefix: &str, config: ChildConfig) {
    let mut rng = Rng(seed);
    report("opening");
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
    let mut known = Known::of(&db.snapshot());
    commit(&mut db);
    let (go, gone) = control();
    report("ready");
    if go.recv().is_err() {
        return;
    }
    let (mut n, mut fresh, mut created) = (0_u64, 0_u64, 0_u64);
    while !gone.load(Ordering::Relaxed) {
        if config.limit > 0 && n >= config.limit {
            break;
        }
        if rng.chance(config.compact_percent) {
            report("begin compact");
            let done = db.compact().unwrap();
            report(&format!("compacted {}", done.seq));
            continue;
        }
        if known.collections.len() < MAX_COLLECTIONS && rng.chance(config.create_percent) {
            created += 1;
            let name = format!("c-{prefix}-{created}");
            report(&format!("begin create {name}"));
            let seq = db.create_collection(schema(&name)).unwrap();
            report(&format!("created {name} {seq}"));
            known.collections.push(name);
            continue;
        }
        let durability = if rng.chance(config.async_percent) {
            Durability::Async
        } else {
            Durability::Fsync
        };
        let token = format!("{prefix}t{n}");
        let op = next_op(&mut rng, &known, prefix, &mut fresh, token, durability);
        report(&format!("op {n} {}", op.encode()));
        match &op {
            Op::Group {
                collection,
                prefix,
                n,
                size,
                token,
                durability,
            } => {
                let docs: Vec<Document> = (0..*size)
                    .map(|i| doc(&group_member(prefix, *n, *size, i), token))
                    .collect();
                db.put_batch(collection, &docs, *durability).unwrap();
            }
            Op::Put {
                collection,
                id,
                token,
                durability,
            } => db.put(collection, &doc(id, token), *durability).unwrap(),
            Op::Delete {
                collection,
                id,
                durability,
            } => db.delete(collection, id, *durability).unwrap(),
        }
        report(&format!("acked {n}"));
        known.apply(&op);
        n += 1;
        if config.commit_every > 0 && n % config.commit_every == 0 {
            commit(&mut db);
            if rng.chance(config.merge_percent) {
                merge_step(&mut db);
            }
        }
    }
    report("done");
}

// ---------------------------------------------------------------------------------------------------------------
// The reader's threads
// ---------------------------------------------------------------------------------------------------------------

/// What the refresher and the viewer found, shared with the parent.
#[derive(Debug, Default)]
struct Shared {
    /// Broken invariants, with their context; the parent fails on the first.
    failures: Mutex<Vec<String>>,
    unchanged: AtomicU64,
    incremental: AtomicU64,
    incremental_records: AtomicU64,
    reloaded: AtomicU64,
    snapshots: AtomicU64,
    /// The largest number of documents one checked snapshot showed.
    max_docs: AtomicU64,
}

impl Shared {
    fn fail(&self, msg: String) {
        let mut f = self.failures.lock().unwrap();
        if f.len() < 20 {
            f.push(msg);
        }
    }

    fn assert_ok(&self, ctx: &str) {
        let f = self.failures.lock().unwrap();
        assert!(
            f.is_empty(),
            "{ctx}: the reader's threads failed:\n{}",
            f.join("\n")
        );
    }

    /// Counts a refresh `outcome` and checks it against the `seq` before (invariants 1, 3): returns the `seq` after.
    fn refreshed(
        &self,
        ro: &ReadOnlyDatabase,
        outcome: &Result<Refreshed, DatabaseError>,
        last: u64,
        who: &str,
    ) -> u64 {
        match outcome {
            Ok(Refreshed::Unchanged) => {
                self.unchanged.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Refreshed::Incremental { seq, records }) => {
                self.incremental.fetch_add(1, Ordering::Relaxed);
                self.incremental_records
                    .fetch_add(*records, Ordering::Relaxed);
                if *seq < last {
                    self.fail(format!(
                        "{who}: an incremental refresh went from seq {last} to {seq}"
                    ));
                }
            }
            Ok(Refreshed::Reloaded { seq }) => {
                self.reloaded.fetch_add(1, Ordering::Relaxed);
                if *seq < last {
                    self.fail(format!("{who}: a reload went from seq {last} to {seq}"));
                }
            }
            Ok(other) => self.fail(format!("{who}: unknown refresh outcome {other:?}")),
            Err(e) => self.fail(format!(
                "{who}: refresh failed (is_corruption: {}, refused: {}): {e} ({e:?})",
                e.is_corruption(),
                matches!(e, DatabaseError::RefreshRefused { .. })
            )),
        }
        let seq = ro.seq();
        if seq < last {
            self.fail(format!(
                "{who}: seq went from {last} to {seq} over a refresh"
            ));
        }
        seq.max(last)
    }
}

/// Sets the flag when dropped, also on a panic: the reader's threads stop, so the scope can join them.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The refresher thread: refreshes until `stop` (invariants 1, 3).
fn refresher(ro: &ReadOnlyDatabase, shared: &Shared, stop: &AtomicBool) {
    let mut last = ro.seq();
    while !stop.load(Ordering::Relaxed) {
        let outcome = ro.refresh();
        last = shared.refreshed(ro, &outcome, last, "refresher");
        std::thread::yield_now();
    }
}

/// Checks one collection snapshot (invariant 2): one view's documents are whole groups with one token each and the
/// documents their ids and tokens stand for; a term search for one visible group's token finds exactly the documents
/// showing it; a `get` answers. Returns the documents checked.
fn check_view(rng: &mut Rng, name: &str, c: &CollectionSnapshot) -> Result<usize, String> {
    let memtable = c.memtable();
    let docs = view_documents(c, &memtable)?;
    let mut groups: BTreeMap<(String, u64, u64), BTreeMap<u64, String>> = BTreeMap::new();
    let mut by_token: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (id, d) in &docs {
        let token = token_of(d).ok_or_else(|| format!("{id}: no body token in {d:?}"))?;
        if *d != doc(id, token) {
            return Err(format!("{id} is not the document of token {token}: {d:?}"));
        }
        by_token
            .entry(token.to_owned())
            .or_default()
            .insert(id.clone());
        if let Some(group) = group_of(id) {
            let i: u64 = id.rsplit_once("-m").unwrap().1.parse().unwrap();
            groups.entry(group).or_default().insert(i, token.to_owned());
        }
    }
    for ((prefix, n, size), members) in &groups {
        let tokens: BTreeSet<&String> = members.values().collect();
        if members.len() as u64 != *size || tokens.len() != 1 {
            return Err(format!(
                "group {prefix}-g{n} of {size} shows members {members:?}: part of a batch"
            ));
        }
    }
    if let Some((_, members)) = groups
        .iter()
        .nth((rng.next_u64() % (groups.len() as u64).max(1)) as usize)
    {
        let token = members.values().next().unwrap();
        let query = Query::Term(TermQuery {
            field: body_field(),
            term: token.clone(),
        });
        let plan = SearchPlan::new(&query, c.schema(), &c.search_statistics(&memtable))
            .map_err(|e| format!("plan for {token}: {e}"))?;
        let hits = c
            .views(&memtable)
            .map_err(|e| format!("views: {e}"))?
            .top_k(&plan, 10_000, Pruning::default())
            .map_err(|e| format!("search for {token}: {e}"))?;
        let mut found = BTreeSet::new();
        for hit in hits {
            let id = match hit.location {
                Located::Memtable(docid) => memtable
                    .document(docid)
                    .map_err(|e| format!("hit memtable document {docid}: {e}"))?
                    .id()
                    .to_owned(),
                Located::Segment { index, docid } => c.segments()[index]
                    .id_of(docid)
                    .map_err(|e| format!("hit segment {index} id {docid}: {e}"))?,
            };
            found.insert(id);
        }
        if found != by_token[token] {
            return Err(format!(
                "a search for {token} found {found:?}, the view shows it on {:?}",
                by_token[token]
            ));
        }
    }
    if let Some(id) = docs
        .keys()
        .nth((rng.next_u64() % (docs.len() as u64).max(1)) as usize)
    {
        // A `get` takes its own memtable view: it may see later records, never fail or lose the id.
        let got = c.get(id).map_err(|e| format!("get {id}: {e}"))?;
        if got.is_none() && group_of(id).is_some() {
            return Err(format!(
                "get {id} in {name}: a visible group member vanished"
            ));
        }
    }
    Ok(docs.len())
}

/// The viewer thread: takes snapshots until `stop` and checks each (invariants 2, 3).
fn viewer(ro: &ReadOnlyDatabase, shared: &Shared, stop: &AtomicBool, seed: u64) {
    let mut rng = Rng(seed);
    let mut last = ro.seq();
    while !stop.load(Ordering::Relaxed) {
        let snapshot = ro.snapshot();
        let seq = snapshot.seq();
        if seq < last {
            shared.fail(format!("viewer: snapshot seq went from {last} to {seq}"));
        }
        last = last.max(seq);
        let mut total = 0;
        for (name, c) in snapshot.collections() {
            match check_view(&mut rng, name, c) {
                Ok(n) => total += n,
                Err(e) => shared.fail(format!("viewer, seq {seq}, collection {name}: {e}")),
            }
        }
        shared.snapshots.fetch_add(1, Ordering::Relaxed);
        shared.max_docs.fetch_max(total as u64, Ordering::Relaxed);
        std::thread::yield_now();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The parent
// ---------------------------------------------------------------------------------------------------------------

/// When the parent kills a writer.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
enum Kill {
    /// A delay after `opening`: before, inside or after the open or its first commit.
    InOpen(Duration),
    /// The delay after the `acks`th acknowledgement, or after the `begins`th `begin` (commit, merge, compaction,
    /// created collection) since `go` if that comes first.
    After {
        acks: u64,
        begins: Option<u64>,
        delay: Duration,
    },
    /// Never: the writer stops by itself after its limit.
    Never,
}

#[cfg(unix)]
fn spawn_writer(path: &Path, seed: u64, prefix: &str, config: &ChildConfig) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, "writer")
        .env(PATH_ENV, path)
        .env(SEED_ENV, seed.to_string())
        .env(PREFIX_ENV, prefix)
        .env(CONFIG_ENV, config.encode())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

/// What one writer reported.
#[cfg(unix)]
#[derive(Debug, Default)]
struct Run {
    opened: bool,
    ready: bool,
    done: bool,
    /// The acknowledged writes, in order.
    acked: Vec<Op>,
    /// The write it reported as started but not as acknowledged: it may or may not have taken effect, as a whole.
    in_flight: Option<Op>,
    /// `begin` reports since `ready`.
    begins: u64,
    /// The last `seq` a commit, merge commit, compaction or created collection returned.
    commit: Option<u64>,
    commits: u64,
    merges: u64,
    compactions: u64,
    created: u64,
    /// The `begin` it reported last without its end: what the kill cut.
    cut: Option<String>,
}

#[cfg(unix)]
impl Run {
    fn record(&mut self, msg: &str, ctx: &str) {
        let (verb, rest) = msg.split_once(' ').unwrap_or((msg, ""));
        let committed = |run: &mut Self, seq: &str| {
            run.commit = run.commit.max(Some(seq.parse().unwrap()));
        };
        match verb {
            "opening" => {}
            "opened" => self.opened = true,
            "ready" => self.ready = true,
            "done" => self.done = true,
            "op" => {
                let (n, op) = rest.split_once(' ').unwrap();
                let n: usize = n.parse().unwrap();
                assert_eq!(n, self.acked.len(), "{ctx}: ops out of order");
                assert!(self.in_flight.is_none(), "{ctx}: two ops in flight");
                self.in_flight = Some(Op::decode(op));
            }
            "acked" => {
                let n: usize = rest.parse().unwrap();
                assert_eq!(n, self.acked.len(), "{ctx}: acks out of order");
                let op = self
                    .in_flight
                    .take()
                    .unwrap_or_else(|| panic!("{ctx}: ack {n} of no op"));
                self.acked.push(op);
            }
            "begin" => {
                if self.ready {
                    self.begins += 1;
                }
                self.cut = Some(rest.to_owned());
            }
            "commit" => {
                self.commits += 1;
                self.cut = None;
                committed(self, rest);
            }
            "merge" => {
                self.merges += 1;
                committed(self, rest);
            }
            "merged" => self.cut = None,
            "compacted" => {
                self.compactions += 1;
                self.cut = None;
                committed(self, rest);
            }
            "created" => {
                self.created += 1;
                self.cut = None;
                committed(self, rest.split_once(' ').unwrap().1);
            }
            other => panic!("{ctx}: unknown child report {other}"),
        }
    }

    fn should_kill(&self, kill: Kill, msg: &str) -> Option<Duration> {
        match kill {
            Kill::InOpen(delay) if msg == "opening" => Some(delay),
            Kill::After {
                acks,
                begins,
                delay,
            } if self.ready
                && (self.acked.len() as u64 >= acks
                    || begins.is_some_and(|b| msg.starts_with("begin ") && self.begins > b)) =>
            {
                Some(delay)
            }
            _ => None,
        }
    }
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

fn describe(d: Option<&Document>) -> String {
    match d {
        None => "absent".to_owned(),
        Some(d) => format!("{d:?}").chars().take(160).collect(),
    }
}

/// The document a model state stands for.
fn expected(key: &Key, state: Option<&Option<String>>) -> Option<Document> {
    state
        .and_then(|s| s.as_deref())
        .map(|token| doc(&key.1, token))
}

/// Checks `present` (one snapshot's documents) against `model` (invariants 4-6): every key as the model says, nothing
/// else visible, and `in_flight` either wholly applied or not at all. Returns the in-flight write's states as
/// `present` resolves it, to apply to the model.
fn check_model(
    present: &BTreeMap<Key, Document>,
    model: &Model,
    in_flight: Option<&Op>,
    ctx: &str,
) -> Vec<(Key, Option<String>)> {
    let effects = in_flight.map(Op::effects).unwrap_or_default();
    let flight_keys: BTreeSet<&Key> = effects.iter().map(|(k, _)| k).collect();
    for (key, state) in model {
        if flight_keys.contains(key) {
            continue;
        }
        let actual = present.get(key);
        let want = expected(key, Some(state));
        assert!(
            actual == want.as_ref(),
            "{ctx}: {key:?} should be {}, but is {}",
            describe(want.as_ref()),
            describe(actual)
        );
    }
    for key in present.keys() {
        assert!(
            model.contains_key(key) || flight_keys.contains(key),
            "{ctx}: {key:?} is visible but was never written (phantom)"
        );
    }
    if effects.is_empty() {
        return Vec::new();
    }
    let applied = effects
        .iter()
        .all(|(k, s)| present.get(k) == expected(k, Some(s)).as_ref());
    let before = effects
        .iter()
        .all(|(k, _)| present.get(k) == expected(k, model.get(k)).as_ref());
    assert!(
        applied || before,
        "{ctx}: the in-flight {in_flight:?} took effect in part: {:?}",
        effects
            .iter()
            .map(|(k, _)| (k, describe(present.get(k))))
            .collect::<Vec<_>>()
    );
    if applied {
        effects
    } else {
        effects
            .into_iter()
            .filter_map(|(k, _)| model.get(&k).map(|s| (k.clone(), s.clone())))
            .collect()
    }
}

/// Invariant 4 / 6: `ro` (just refreshed, the file still) equals a fresh read-only open of `path`.
#[cfg(unix)]
fn assert_like_fresh(ro: &ReadOnlyDatabase, path: &Path, ctx: &str) -> BTreeMap<Key, Document> {
    let fresh = ReadOnlyDatabase::open(path, &AnalyzerRegistry::builtin())
        .unwrap_or_else(|e| panic!("{ctx}: a fresh read-only open failed: {e}"));
    assert_eq!(ro.seq(), fresh.seq(), "{ctx}: seq");
    assert!(
        *ro.manifest() == *fresh.manifest(),
        "{ctx}: the manifests differ:\n{:?}\n{:?}",
        ro.manifest(),
        fresh.manifest()
    );
    assert_eq!(ro.replayed(), fresh.replayed(), "{ctx}: replayed records");
    assert_eq!(ro.wal_end(), fresh.wal_end(), "{ctx}: WAL end");
    let (mine, theirs) = (ro.snapshot(), fresh.snapshot());
    let names = |s: &Snapshot| {
        s.collections()
            .map(|(n, _)| n.to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&mine), names(&theirs), "{ctx}: collections");
    for (name, c) in theirs.collections() {
        let m = mine.collection(name).unwrap();
        let ids = |c: &CollectionSnapshot| {
            c.segments()
                .iter()
                .map(|s| s.segment_id())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(m), ids(c), "{ctx}: segments of {name}");
        assert_eq!(
            m.memtable().live_count(),
            c.memtable().live_count(),
            "{ctx}: memtable of {name}"
        );
    }
    let present = documents(&mine);
    assert!(
        present == documents(&theirs),
        "{ctx}: the refreshed reader and a fresh open show other documents"
    );
    present
}

/// Counts over the run.
#[cfg(unix)]
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    writers: u64,
    kills: u64,
    kills_in_open: u64,
    /// Kills while a commit, merge, compaction or created collection had begun and not ended.
    kills_cut: u64,
    fsync_writes: u64,
    async_writes: u64,
    commits: u64,
    merges: u64,
    compactions: u64,
    created: u64,
    docs: usize,
}

/// One file: create it, open the reader, run `rounds` writers (all but the last killed) beside the reader's
/// threads, checking after each.
#[cfg(unix)]
fn rounds_on_one_file(rng: &mut Rng, rounds: u64, ctx: &str) -> (Stats, Arc<Shared>) {
    let dir = Artifacts(Some(tempfile::tempdir().unwrap()));
    let path = dir.path().join("app.persia");
    let after_kill = dir.path().join("after-kill.persia");
    {
        let mut db = Database::create(&path, options(rng.next_u64())).unwrap();
        db.create_collection(schema(FIRST_COLLECTION)).unwrap();
    }
    let ro = ReadOnlyDatabase::open(&path, &AnalyzerRegistry::builtin()).unwrap();
    let shared = Arc::new(Shared::default());
    let stop = AtomicBool::new(false);
    let viewer_seed = rng.next_u64();
    let mut stats = Stats::default();
    let mut model = Model::new();
    std::thread::scope(|s| {
        s.spawn(|| refresher(&ro, &shared, &stop));
        s.spawn(|| viewer(&ro, &shared, &stop, viewer_seed));
        let _stop = StopOnDrop(&stop);
        let mut last_seq = ro.seq();
        let mut last_commit: Option<u64> = None;
        for round in 0..rounds {
            let last = round + 1 == rounds;
            let config = ChildConfig {
                commit_every: if rng.chance(15) { 0 } else { rng.range(1, 12) },
                merge_percent: rng.range(30, 90),
                compact_percent: if rng.chance(50) { rng.range(1, 5) } else { 0 },
                create_percent: rng.range(0, 5),
                async_percent: rng.range(0, 50),
                limit: if last { rng.range(20, 60) } else { 0 },
            };
            let kill = if last {
                Kill::Never
            } else if rng.chance(12) {
                Kill::InOpen(Duration::from_micros(rng.range(0, 20_000)))
            } else {
                Kill::After {
                    acks: rng.range(5, 90),
                    begins: rng.chance(50).then(|| rng.range(0, 6)),
                    delay: Duration::from_micros(rng.range(0, 2500)),
                }
            };
            let child_seed = rng.next_u64();
            let ctx = format!("{ctx}, round {round} ({config:?}, {kill:?})");
            let prefix = format!("r{round}");
            let mut child = spawn_writer(&path, child_seed, &prefix, &config);
            let mut stdin = child.stdin.take();
            let mut out = BufReader::new(child.stdout.take().unwrap());
            let (tx, rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                while let Some(msg) = next_tagged(&mut out) {
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            });
            let mut run = Run::default();
            let mut killed = false;
            while let Ok(msg) = rx.recv() {
                run.record(&msg, &ctx);
                if msg == "ready" {
                    // Invariant 5: the restarted writer's first commit, then one refresh, shows the model.
                    let outcome = ro.refresh();
                    last_seq = shared.refreshed(&ro, &outcome, last_seq, "parent, after ready");
                    shared.assert_ok(&ctx);
                    check_model(
                        &documents(&ro.snapshot()),
                        &model,
                        None,
                        &format!("{ctx}, after the writer's first commit"),
                    );
                    assert_eq!(
                        Some(ro.seq()),
                        run.commit,
                        "{ctx}: the reader's seq after ready is not the writer's first commit"
                    );
                    assert!(
                        last_commit.is_none_or(|c| ro.seq() >= c),
                        "{ctx}: seq {} after ready, the last commit was {last_commit:?}",
                        ro.seq()
                    );
                    let go = stdin.as_mut().unwrap();
                    go.write_all(b"go\n").unwrap();
                    go.flush().unwrap();
                }
                if let Some(delay) = run.should_kill(kill, &msg) {
                    std::thread::sleep(delay);
                    child.kill().unwrap(); // SIGKILL on Unix
                    killed = true;
                    break;
                }
            }
            let exit = child.wait().unwrap();
            drop(stdin);
            reader.join().unwrap();
            for msg in rx.try_iter() {
                run.record(&msg, &ctx);
            }
            if killed {
                assert!(!exit.success(), "{ctx}: writer exited by itself: {exit}");
                stats.kills += 1;
                if !run.ready {
                    stats.kills_in_open += 1;
                }
                if run.cut.is_some() {
                    stats.kills_cut += 1;
                }
            } else {
                assert!(
                    exit.success() && run.done,
                    "{ctx}: writer failed (done: {}, acked {}): {exit}",
                    run.done,
                    run.acked.len()
                );
                assert!(
                    run.in_flight.is_none(),
                    "{ctx}: done with a write in flight"
                );
            }
            stats.writers += 1;
            for op in &run.acked {
                match op.durability() {
                    Durability::Fsync => stats.fsync_writes += 1,
                    _ => stats.async_writes += 1,
                }
                model.extend(op.effects());
            }
            last_commit = last_commit.max(run.commit);
            stats.commits += run.commits;
            stats.merges += run.merges;
            stats.compactions += run.compactions;
            stats.created += run.created;
            if killed {
                std::fs::copy(&path, &after_kill).unwrap();
            }
            // Invariant 4 (or 6): the file is still; one refresh shows every acknowledged write and equals a fresh
            // open.
            let ctx = format!(
                "{ctx}, after {} (in flight {:?}, cut {:?})",
                if killed { "the kill" } else { "the clean exit" },
                run.in_flight,
                run.cut
            );
            let outcome = ro.refresh();
            last_seq = shared.refreshed(&ro, &outcome, last_seq, "parent, after the writer");
            shared.assert_ok(&ctx);
            let present = assert_like_fresh(&ro, &path, &ctx);
            let resolved = check_model(&present, &model, run.in_flight.as_ref(), &ctx);
            model.extend(resolved);
            if let Some(c) = last_commit {
                assert!(
                    ro.seq() >= c,
                    "{ctx}: seq {} < the last commit {c}",
                    ro.seq()
                );
            }
            if std::env::var_os("PERSIA_TEST_VERBOSE").is_some() {
                eprintln!(
                    "{ctx}: seq {}, {} acked, {} documents",
                    ro.seq(),
                    run.acked.len(),
                    present.len()
                );
            }
            stats.docs = present.len();
        }
        // Invariant 6, with the threads stopped: the reader equals a fresh open and the model, exactly.
        stop.store(true, Ordering::Relaxed);
    });
    shared.assert_ok(ctx);
    let outcome = ro.refresh();
    shared.refreshed(&ro, &outcome, ro.seq(), "parent, final");
    shared.assert_ok(ctx);
    let present = assert_like_fresh(&ro, &path, &format!("{ctx}, final"));
    check_model(&present, &model, None, &format!("{ctx}, final"));
    ro.verify()
        .unwrap_or_else(|e| panic!("{ctx}: verify of the refreshed reader failed: {e}"));
    ReadOnlyDatabase::open(&path, &AnalyzerRegistry::builtin())
        .unwrap()
        .verify()
        .unwrap_or_else(|e| panic!("{ctx}: verify of a fresh open failed: {e}"));
    drop(dir);
    (stats, shared)
}

/// Writers put, upsert and delete (`Fsync`, `Async`, batches), commit, merge, create collections and compact, and
/// are killed with SIGKILL, while ONE read-only database refreshes in a loop and another thread checks its snapshots;
/// see the module docs for the invariants.
#[cfg(unix)]
#[test]
fn one_reader_refreshes_beside_killed_writers() {
    let seed = base_seed();
    let files = env_count("PERSIA_TEST_FILES", FILES);
    let rounds = env_count("PERSIA_TEST_ROUNDS", ROUNDS).max(1);
    let mut rng = Rng(seed);
    let started = std::time::Instant::now();
    let mut total = Stats::default();
    let (mut unchanged, mut incremental, mut records, mut reloaded) = (0, 0, 0, 0);
    let (mut snapshots, mut max_docs) = (0, 0);
    for file in 0..files {
        let (s, shared) =
            rounds_on_one_file(&mut rng, rounds, &format!("seed {seed}, file {file}"));
        total.writers += s.writers;
        total.kills += s.kills;
        total.kills_in_open += s.kills_in_open;
        total.kills_cut += s.kills_cut;
        total.fsync_writes += s.fsync_writes;
        total.async_writes += s.async_writes;
        total.commits += s.commits;
        total.merges += s.merges;
        total.compactions += s.compactions;
        total.created += s.created;
        total.docs += s.docs;
        unchanged += shared.unchanged.load(Ordering::Relaxed);
        incremental += shared.incremental.load(Ordering::Relaxed);
        records += shared.incremental_records.load(Ordering::Relaxed);
        reloaded += shared.reloaded.load(Ordering::Relaxed);
        snapshots += shared.snapshots.load(Ordering::Relaxed);
        max_docs = max_docs.max(shared.max_docs.load(Ordering::Relaxed));
    }
    eprintln!(
        "read-only refresh process loop: {} writers on {files} files, {} killed ({} in the open, {} inside a commit, \
         merge, compaction or collection creation); acknowledged {} Fsync and {} Async writes; {} commits, {} merge \
         commits, {} compactions, {} created collections; refreshes: {unchanged} unchanged, {incremental} \
         incremental ({records} records), {reloaded} reloaded; {snapshots} snapshots checked (up to {max_docs} \
         documents); {} documents at the end; {:?}",
        total.writers,
        total.kills,
        total.kills_in_open,
        total.kills_cut,
        total.fsync_writes,
        total.async_writes,
        total.commits,
        total.merges,
        total.compactions,
        total.created,
        total.docs,
        started.elapsed()
    );
    // The scenario must reach both kinds of refresh, or it tests less than it says.
    assert!(
        incremental > 0,
        "seed {seed}: no incremental refresh happened"
    );
    assert!(reloaded > 0, "seed {seed}: no reload happened");
}

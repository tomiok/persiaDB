//! A read-only reader process refreshing beside a writer process that puts, deletes, commits and rewrites the pack
//! (GC), with the writer killed (SIGKILL) at seeded points (SPEC §8.4, §8.6, §9.3, §9.5, §18.10; ROADMAP 9.4.5,
//! 9.5.4; `ReadOnlyBlobDatabase::refresh` and `ReadOnlyDatabase::wal_end`). The children are this test binary
//! re-executed (`blob_support`); they report each step on stdout and wait on stdin, so no test sleeps.
//!
//! The writer runs free: a seeded schedule of `put` (`Fsync`, `Async` or `Buffered`, a blob per document, ids unique
//! per writer process), `delete` (`Fsync`) of a live document, `commit`, and `gc` with a forced pack rewrite. The
//! reader takes commands at seeded writer reports: `open` (a new `ReadOnlyDatabase::open`, held), and `refresh`
//! with the newest or the oldest held open, so an open is often refreshed long after it was taken (the window in
//! which GC sweeps a deleted document's blob). After a kill the reader opens and refreshes again, `persia verify`
//! checks the files as the kill left them, and a new writer process recovers them and goes on.
//!
//! Invariants:
//! 1. Opens taken one after another on one history are ordered: a higher `seq`, or the same `seq` with the replayed
//!    WAL records and `wal_end` both greater or both equal. Never disagreeing (what `RefreshDiverged` reports), across
//!    kills and recoveries too.
//! 2. A refresh with an open taken after the current one is always taken; with an earlier one it is taken only in the
//!    same state, else `RefreshedOlder`. No other error, never `RefreshDiverged`, never a panic.
//! 3. A taken refresh shows exactly the open's `seq`, records and WAL end; the pack never goes back (its generation,
//!    then its commit `seq`); a refused one changes nothing (documents, WAL position, pack).
//! 4. Every blob a shown document references reads back byte-identical, never `BlobNotFound`: the reader holds a
//!    reader lease, so no GC sweeps while its documents are behind the writer's commits (SPEC §9.3, §9.5; ROADMAP
//!    9.2.16). A document first shown, and every document after the pack changed, is read whole.
//! 5. An open shows every `Fsync` and `Async` put acknowledged before it (SIGKILL keeps the page cache) and every
//!    document an earlier open showed, unless the writer started deleting it; it shows no document whose delete was
//!    acknowledged before it, and no document no writer started.
//! 6. After every kill `persia verify` passes; at the end `--deep` passes, and a fresh open shows what the reader
//!    shows, with no blob missing.
//!
//! Invariant 4's old window (a GC between a delete and the refresh) is also driven step by step (a writer that runs
//! one command per stdin line), so a default run always reaches it: GC is held back while the reader is behind. That stepped run also
//! forces a pack rewrite and a `RefreshedOlder` refusal. Whether the seeded schedule reaches either is printed and
//! does not fail the test.
//!
//! Seeds: `PERSIA_TEST_SEED` (printed) sets the schedules; `PERSIA_TEST_FILES`, `PERSIA_TEST_KILLS` and
//! `PERSIA_TEST_OPS` override the files, the kills per file and the operations per writer; `PERSIA_TEST_TRACE` prints
//! every writer report. A replay reproduces the
//! schedules (writer operations, kill and command report counts), not the exact instants. On a failure the temp dir is
//! kept and its path printed.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::too_many_lines
)] // Tests: inputs are generated or fixed.

mod blob_support;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, Read};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use blob_support::{
    COLLECTION, ChildIo, KeptOnFailure, Rng, base_seed, child_env, content, env_count, hash_str,
    live_ids, pick_size, report, serial, verify_ok,
};
use persia::internal::documents::{Blob, BlobDatabase, Error, ReadOnlyBlobDatabase};
use persia::internal::gc::GcOptions;
use persia_engine::db::{DatabaseOptions, ReadOnlyDatabase};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::container::ContainerFile;

#[cfg(not(feature = "slow"))]
const FILES: u64 = 1;
#[cfg(feature = "slow")]
const FILES: u64 = 2;

#[cfg(not(feature = "slow"))]
const KILLS_PER_FILE: u64 = 3;
#[cfg(feature = "slow")]
const KILLS_PER_FILE: u64 = 12;

#[cfg(not(feature = "slow"))]
const OPS: u64 = 40;
#[cfg(feature = "slow")]
const OPS: u64 = 80;

/// The share of blobs at or above the 4 MiB chunk: kept low, the reader reads every blob again after a rewrite.
const BIG_PERCENT: u64 = 2;

/// The most opens the reader holds; at the cap the parent refreshes with the oldest.
const MAX_HELD: usize = 4;

/// The blob of document `id`: a pure function of the id, as both children and the parent derive it.
fn bytes_of(id: &str) -> Vec<u8> {
    let mut rng = Rng(hash_str(id));
    let size = pick_size(&mut rng, BIG_PERCENT);
    content(size, rng.next_u64())
}

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
        "gc-writer" => gc_writer(&env.path, env.seed, &env.prefix, env.count),
        "refresher" => refresher(&env.path),
        "stepped-writer" => stepped_writer(&env.path),
        other => panic!("unknown child role {other}"),
    }
}

/// Runs `count` seeded operations on `path`: `put <prefix>-<n>`, `delete` of a live document, `commit`, or `gc` with
/// a forced rewrite. Reports `opening`, `opened`, `start put <id>` / `ack put <id> <durability>`, `start del <id>` /
/// `ack del <id>`, `commit <seq>`, `start gc` / `gc <blobs deleted> <rewrite>`, then `done`; stops early once stdin
/// closes.
fn gc_writer(path: &Path, seed: u64, prefix: &str, count: u64) {
    let (gone, eof) = parent_gone_signal();
    let mut rng = Rng(seed);
    report("opening");
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    let mut live: Vec<String> = live_ids(&db.log().snapshot(), COLLECTION)
        .into_iter()
        .collect();
    report("opened");
    for n in 0..count {
        if gone.load(Ordering::Relaxed) {
            return;
        }
        let roll = rng.range(0, 100);
        if roll < 55 || live.is_empty() {
            let id = format!("{prefix}-{n}");
            let (durability, tag) = match rng.range(0, 10) {
                0..=6 => (Durability::Fsync, "fsync"),
                7 | 8 => (Durability::Async, "async"),
                _ => (Durability::Buffered, "buffered"),
            };
            report(&format!("start put {id}"));
            db.put(
                COLLECTION,
                &Document::new(&id),
                Blob::from_reader(&bytes_of(&id)[..]),
                durability,
            )
            .unwrap();
            report(&format!("ack put {id} {tag}"));
            live.push(id);
        } else if roll < 72 {
            let i = usize::try_from(rng.range(0, u64::try_from(live.len()).unwrap())).unwrap();
            let id = live.swap_remove(i);
            report(&format!("start del {id}"));
            db.log_mut()
                .delete(COLLECTION, &id, Durability::Fsync)
                .unwrap();
            db.release_durable();
            report(&format!("ack del {id}"));
        } else if roll < 86 {
            let seq = db.commit().unwrap();
            report(&format!("commit {seq}"));
        } else {
            report("start gc");
            let gc = db
                .gc(&GcOptions {
                    rewrite_ratio: 0.0,
                    ..GcOptions::default()
                })
                .unwrap();
            let rewrite = match &gc.sweep.rewrite {
                Some(r) => format!("{r:?}")
                    .split([' ', '{'])
                    .next()
                    .unwrap()
                    .to_owned(),
                None => "none".to_owned(),
            };
            report(&format!("gc {} {rewrite}", gc.sweep.blobs_deleted));
        }
    }
    report("done");
    // Exit only once the parent closes stdin, so a kill aimed at the last reports never finds the process gone.
    eof.recv().unwrap();
}

/// As [`parent_gone`], plus a channel that receives once stdin reached EOF, for a child that waits on it.
fn parent_gone_signal() -> (
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::sync::mpsc::Receiver<()>,
) {
    let gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&gone);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let ended = std::io::stdin().read_to_end(&mut sink);
        flag.store(true, Ordering::Relaxed);
        tx.send(()).unwrap();
        ended.unwrap();
    });
    (gone, rx)
}

/// Runs one command per stdin line on `path` and reports `ok <line>` after it: `put <id>` (`Fsync`), `del <id>`
/// (`Fsync`), `commit`, or `gc` (a forced rewrite; reports `ok gc <blobs deleted> <rewrite>`).
fn stepped_writer(path: &Path) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    report("opened");
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        match line.split_once(' ') {
            Some(("put", id)) => {
                db.put(
                    COLLECTION,
                    &Document::new(id),
                    Blob::from_reader(&bytes_of(id)[..]),
                    Durability::Fsync,
                )
                .unwrap();
                report(&format!("ok {line}"));
            }
            Some(("del", id)) => {
                db.log_mut()
                    .delete(COLLECTION, id, Durability::Fsync)
                    .unwrap();
                db.release_durable();
                report(&format!("ok {line}"));
            }
            None if line == "commit" => {
                db.commit().unwrap();
                report("ok commit");
            }
            None if line == "gc" => {
                let gc = db
                    .gc(&GcOptions {
                        rewrite_ratio: 0.0,
                        ..GcOptions::default()
                    })
                    .unwrap();
                let rewrite = format!("{:?}", gc.sweep.rewrite);
                report(&format!(
                    "ok gc {} {}",
                    gc.sweep.blobs_deleted,
                    rewrite.contains("Done")
                ));
            }
            _ => panic!("stepped writer: unknown command {line}"),
        }
    }
}

/// What a reader shows, compared before and after a refused refresh.
#[derive(Debug, PartialEq, Eq)]
struct Shown {
    seq: u64,
    replayed: u64,
    wal_end: u64,
    /// The pack's `(generation, seq, log_end, pack_id)`, zeros with no pack.
    pack: (u64, u64, u64, u128),
    ids: BTreeSet<String>,
}

fn shown(ro: &ReadOnlyBlobDatabase) -> Shown {
    let db = ro.database();
    Shown {
        seq: db.seq(),
        replayed: db.replayed(),
        wal_end: db.wal_end(),
        pack: ro.blobs().store().superblock().map_or((0, 0, 0, 0), |sb| {
            (sb.generation, sb.seq, sb.log_end, sb.pack_id)
        }),
        ids: live_ids(&db.snapshot(), COLLECTION),
    }
}

fn kind(e: &Error) -> &'static str {
    match e {
        Error::RefreshedOlder { .. } => "older",
        Error::RefreshDiverged { .. } => "diverged",
        Error::RefreshedElsewhere { .. } => "elsewhere",
        Error::Blob(_) => "blob",
        Error::Database(_) => "database",
        _ => "other",
    }
}

/// Opens `path` read-only, then takes commands on stdin:
/// - `open`: a new `ReadOnlyDatabase::open`, held; reports `held <seq> <replayed> <wal_end>` (or `open-error <e>`);
/// - `refresh newest|oldest`: refreshes with that held open; reports `taken <found> <seq> <replayed> <wal_end>
///   <generation> <pack seq> ids=<a,b> missing=<c> read=<bytes>` or `refused <kind> <unchanged> <e>`. A blob that
///   reads wrong (other than `BlobNotFound`) is reported as `bad <id> <what>`.
fn refresher(path: &Path) {
    let options = DatabaseOptions::default();
    let mut ro = ReadOnlyBlobDatabase::open(path, &options).unwrap();
    let mut held: VecDeque<ReadOnlyDatabase> = VecDeque::new();
    // The ids read whole in the pack now open: `pack_id` and generation.
    let mut read_in: (u128, u64) = (0, 0);
    let mut read: BTreeSet<String> = BTreeSet::new();
    report("ready");
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        match line.as_str() {
            "open" => match ReadOnlyDatabase::open(path, &options.registry) {
                Ok(db) => {
                    report(&format!(
                        "held {} {} {}",
                        db.seq(),
                        db.replayed(),
                        db.wal_end()
                    ));
                    held.push_back(db);
                }
                Err(e) => report(&format!("open-error {e}")),
            },
            "refresh newest" | "refresh oldest" => {
                let reopened = if line.ends_with("newest") {
                    held.pop_back()
                } else {
                    held.pop_front()
                }
                .unwrap();
                let opened = (reopened.seq(), reopened.replayed(), reopened.wal_end());
                let before = shown(&ro);
                match ro.refresh(reopened) {
                    Ok(found) => {
                        let now = shown(&ro);
                        let mut bad: Vec<String> = Vec::new();
                        if (now.seq, now.replayed, now.wal_end) != opened {
                            bad.push(format!(
                                "shows {:?}, the open was {opened:?}",
                                (now.seq, now.replayed, now.wal_end)
                            ));
                        }
                        let pack = (now.pack.3, now.pack.0);
                        if pack != read_in {
                            read_in = pack;
                            read.clear();
                        }
                        let snapshot = ro.database().snapshot();
                        let c = snapshot.collection(COLLECTION).unwrap();
                        let (mut missing, mut bytes) = (Vec::new(), 0_usize);
                        for id in &now.ids {
                            let doc = c.get(id).unwrap().unwrap();
                            let Some(blob) = doc.blob_ref() else {
                                bad.push(format!("{id} has no blob reference"));
                                continue;
                            };
                            let whole = !read.contains(id);
                            let got = if whole {
                                ro.get_blob(blob).and_then(|mut r| {
                                    let mut out = Vec::new();
                                    r.read_to_end(&mut out).map_err(|source| Error::Open {
                                        path: id.into(),
                                        source,
                                    })?;
                                    Ok(out)
                                })
                            } else {
                                // Read before in this pack: one byte (or none) is enough to see it is still bound.
                                let len = blob.size();
                                ro.get_blob_range(blob, len.saturating_sub(1)..len)
                            };
                            match got {
                                Ok(out) => {
                                    if whole {
                                        if out != bytes_of(id) {
                                            bad.push(format!("{id}: its blob differs"));
                                        }
                                        bytes += out.len();
                                        read.insert(id.clone());
                                    }
                                }
                                Err(Error::Blob(e))
                                    if format!("{e:?}").starts_with("BlobNotFound") =>
                                {
                                    missing.push(id.clone());
                                }
                                Err(e) => bad.push(format!("{id}: {e:?}")),
                            }
                        }
                        let ids: Vec<&str> = now.ids.iter().map(String::as_str).collect();
                        if !bad.is_empty() {
                            report(&format!("bad {}", bad.join("; ")));
                            continue;
                        }
                        report(&format!(
                            "taken {found} {} {} {} {} {} ids={} missing={} read={bytes}",
                            now.seq,
                            now.replayed,
                            now.wal_end,
                            now.pack.0,
                            now.pack.1,
                            ids.join(","),
                            missing.join(","),
                        ));
                    }
                    Err(e) => {
                        let unchanged = shown(&ro) == before;
                        report(&format!("refused {} {unchanged} {e}", kind(&e)));
                    }
                }
            }
            other => panic!("refresher: unknown command {other}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Parent
// ---------------------------------------------------------------------------------------------------------------

/// A database at `<dir>/app.persia` with the collection, committed, and no pack.
fn create(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("app.persia");
    let mut db = BlobDatabase::create(&path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new(COLLECTION).build().unwrap())
        .unwrap();
    db.commit().unwrap();
    path
}

/// What the parent knows of the writers' work, from the reports it received.
#[derive(Debug, Default)]
struct Model {
    put_started: BTreeSet<String>,
    /// Acknowledged `Fsync` and `Async` puts: in the file when acknowledged.
    put_written: BTreeSet<String>,
    del_started: BTreeSet<String>,
    del_acked: BTreeSet<String>,
    /// Every id an open showed, with the index of the first open that showed it.
    seen: BTreeMap<String, u64>,
    gcs: u64,
    rewrites: u64,
    swept: u64,
    commits: u64,
}

impl Model {
    fn record(&mut self, msg: &str, ctx: &str) {
        if std::env::var_os("PERSIA_TEST_TRACE").is_some() {
            eprintln!("{ctx}: {msg}");
        }
        let parts: Vec<&str> = msg.split(' ').collect();
        match parts.as_slice() {
            ["opening" | "opened" | "done"] | ["start", "gc"] => {}
            ["start", "put", id] => {
                assert!(self.put_started.insert((*id).to_owned()), "{ctx}: {id}");
            }
            ["ack", "put", id, durability] => {
                if *durability != "buffered" {
                    self.put_written.insert((*id).to_owned());
                }
            }
            ["start", "del", id] => {
                self.del_started.insert((*id).to_owned());
            }
            ["ack", "del", id] => {
                self.del_acked.insert((*id).to_owned());
            }
            ["commit", _] => self.commits += 1,
            ["gc", swept, rewrite] => {
                self.gcs += 1;
                self.swept += swept.parse::<u64>().unwrap();
                self.rewrites += u64::from(*rewrite == "Done");
            }
            _ => panic!("{ctx}: unexpected writer report {msg:?}"),
        }
    }
}

/// One open the reader holds, with what the parent knew when it asked for it.
#[derive(Debug)]
struct Held {
    index: u64,
    state: (u64, u64, u64),
    put_written: BTreeSet<String>,
    del_acked: BTreeSet<String>,
}

/// Invariant 1: `later` is not older than `earlier` and does not disagree with it.
fn ordered(earlier: (u64, u64, u64), later: (u64, u64, u64)) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match later.0.cmp(&earlier.0) {
        Greater => true,
        Less => false,
        Equal => matches!(
            (later.1.cmp(&earlier.1), later.2.cmp(&earlier.2)),
            (Greater, Greater) | (Equal, Equal)
        ),
    }
}

#[derive(Debug, Default)]
struct Stats {
    opens: u64,
    taken: u64,
    refused_older: u64,
    /// Pack rewrites the writers reported, summed across files.
    rewrites: u64,
    taken_from_older: u64,
    found_new: u64,
    missing_window: u64,
    kills: u64,
    bytes_read: u64,
    refresh_us: Vec<u128>,
}

/// The parent's side of the reader child: the commands it sent and the checks on the replies.
struct Reader {
    io: ChildIo,
    held: VecDeque<Held>,
    next_index: u64,
    /// The index of the open the reader shows, and its state.
    current: (u64, (u64, u64, u64)),
    /// The last open's state (invariant 1).
    last_open: (u64, u64, u64),
    /// The pack's `(generation, seq)` the reader last showed.
    pack: (u64, u64),
    /// Every id a taken refresh found `BlobNotFound`.
    missing: BTreeSet<String>,
    shown_ids: BTreeSet<String>,
    /// The ids the last taken refresh showed.
    last_ids: BTreeSet<String>,
    /// Documents a taken refresh did not show that it should have unless a writer deleted them: (what, id).
    absent: Vec<(String, String)>,
}

impl Reader {
    fn spawn(path: &Path) -> Self {
        let io = ChildIo::spawn("refresher", path, 0, "", 0);
        assert_eq!(io.expect("reader"), "ready");
        Self {
            io,
            held: VecDeque::new(),
            next_index: 1,
            current: (0, (0, 0, 0)),
            last_open: (0, 0, 0),
            pack: (0, 0),
            missing: BTreeSet::new(),
            shown_ids: BTreeSet::new(),
            last_ids: BTreeSet::new(),
            absent: Vec::new(),
        }
    }

    fn open(&mut self, model: &Model, stats: &mut Stats, ctx: &str) {
        self.io.send("open");
        let reply = self.io.expect(ctx);
        let parts: Vec<&str> = reply.split(' ').collect();
        assert_eq!(parts[0], "held", "{ctx}: the reader's open failed: {reply}");
        let opened: (u64, u64, u64) = (
            parts[1].parse().unwrap(),
            parts[2].parse().unwrap(),
            parts[3].parse().unwrap(),
        );
        assert!(
            ordered(self.last_open, opened),
            "{ctx}: open {} (seq, replayed, wal_end) {opened:?} is not ordered after the previous open's {:?}",
            self.next_index,
            self.last_open
        );
        self.last_open = opened;
        self.held.push_back(Held {
            index: self.next_index,
            state: opened,
            put_written: model.put_written.clone(),
            del_acked: model.del_acked.clone(),
        });
        self.next_index += 1;
        stats.opens += 1;
    }

    fn refresh(&mut self, newest: bool, model: &mut Model, stats: &mut Stats, ctx: &str) {
        let held = if newest {
            self.held.pop_back()
        } else {
            self.held.pop_front()
        }
        .unwrap();
        let started = Instant::now();
        self.io.send(if newest {
            "refresh newest"
        } else {
            "refresh oldest"
        });
        let reply = self.io.expect(ctx);
        stats.refresh_us.push(started.elapsed().as_micros());
        assert!(!reply.starts_with("bad "), "{ctx}: {reply}");
        let ctx = format!("{ctx}: refresh with open {} {:?}", held.index, held.state);
        let parts: Vec<&str> = reply.splitn(4, ' ').collect();
        if parts[0] == "refused" {
            assert_eq!(
                parts[2], "true",
                "{ctx}: a refused refresh changed the reader: {reply}"
            );
            assert!(
                parts[1] == "older" && held.index < self.current.0 && held.state != self.current.1,
                "{ctx}: refused while showing open {} {:?}: {reply}",
                self.current.0,
                self.current.1
            );
            stats.refused_older += 1;
            return;
        }
        let parts: Vec<&str> = reply.split(' ').collect();
        assert_eq!(parts[0], "taken", "{ctx}: {reply}");
        let num = |i: usize| -> u64 { parts[i].parse().unwrap() };
        assert_eq!((num(2), num(3), num(4)), held.state, "{ctx}: {reply}");
        if held.index < self.current.0 {
            assert_eq!(
                held.state, self.current.1,
                "{ctx}: an older open was taken: {reply}"
            );
            stats.taken_from_older += 1;
        }
        let pack = (num(5), num(6));
        assert!(
            pack >= self.pack,
            "{ctx}: the pack went back from {:?} to {pack:?}",
            self.pack
        );
        self.pack = pack;
        let list = |field: &str| -> BTreeSet<String> {
            let v = parts.iter().find_map(|p| p.strip_prefix(field)).unwrap();
            v.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let ids = list("ids=");
        let missing = list("missing=");
        stats.bytes_read += parts
            .iter()
            .find_map(|p| p.strip_prefix("read="))
            .unwrap()
            .parse::<u64>()
            .unwrap();
        // Invariant 5. A document absent here may have been deleted by a writer report the parent has not received
        // yet (the delete ran before this open, its report is still in the pipe): checked once the writer is reaped.
        for id in held.put_written.iter().filter(|id| !ids.contains(*id)) {
            self.absent.push((
                format!("{ctx}: lost {id}, acknowledged before the open"),
                id.clone(),
            ));
        }
        for (id, first) in &model.seen {
            if *first < held.index && !ids.contains(id) {
                self.absent.push((
                    format!("{ctx}: lost {id}, shown by open {first}"),
                    id.clone(),
                ));
            }
        }
        for id in &ids {
            assert!(
                !held.del_acked.contains(id),
                "{ctx}: shows {id}, deleted before the open"
            );
            model.seen.entry(id.clone()).or_insert(held.index);
        }
        // Invariant 4: only the documented window. Each missing id must be one the writer deleted: checked at the
        // end, once every writer report arrived.
        stats.missing_window += u64::try_from(missing.len()).unwrap();
        self.missing.extend(missing);
        self.shown_ids.extend(ids.iter().cloned());
        self.last_ids = ids;
        stats.found_new += u64::from(parts[1] == "true");
        stats.taken += 1;
        self.current = (held.index, held.state);
    }

    /// At a writer report: maybe a command, by the schedule.
    fn step(&mut self, rng: &mut Rng, model: &mut Model, stats: &mut Stats, ctx: &str) {
        if !rng.chance(50) {
            return;
        }
        if self.held.len() >= MAX_HELD {
            self.refresh(false, model, stats, ctx);
            return;
        }
        match rng.range(0, 10) {
            5 | 6 if !self.held.is_empty() => self.refresh(true, model, stats, ctx),
            7..=9 if !self.held.is_empty() => self.refresh(false, model, stats, ctx),
            _ => self.open(model, stats, ctx),
        }
    }

    /// Invariant 5, once the writer that ran is reaped: every document a refresh did not show where it should have
    /// was one a writer started deleting.
    fn settle(&mut self, model: &Model) {
        for (what, id) in self.absent.drain(..) {
            assert!(
                model.del_started.contains(&id),
                "{what}, and no writer deleted it"
            );
        }
    }

    /// Opens and refreshes with that open, which must be taken; then refreshes with every open still held (older
    /// ones), each taken in the same state or refused as older.
    fn catch_up(&mut self, model: &mut Model, stats: &mut Stats, ctx: &str) {
        self.open(model, stats, ctx);
        self.refresh(true, model, stats, ctx);
        while !self.held.is_empty() {
            self.refresh(false, model, stats, ctx);
        }
    }
}

fn percentile(sorted: &[u128], p: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(sorted.len() - 1) * p / 100]
}

/// Invariants 1 to 6 over `kills` writer processes killed at seeded reports and a last one that runs to its end.
fn run_file(seed: u64, kills: u64, ops: u64, stats: &mut Stats) {
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    let mut rng = Rng(seed);
    let mut model = Model::default();
    let mut reader = Reader::spawn(&path);
    let started = Instant::now();

    for incarnation in 0..=kills {
        let last = incarnation == kills;
        let prefix = format!("w{incarnation}");
        let ctx = format!("seed {seed} writer {prefix}");
        // Each op makes one or two reports after `opening` / `opened`, so a kill at most `ops + 1` reports in comes
        // before `done`: anywhere from inside the open on.
        let kill_at = (!last).then(|| rng.range(1, ops + 2));
        let writer = ChildIo::spawn("gc-writer", &path, rng.next_u64(), &prefix, ops);
        let mut reports = 0_u64;
        let mut finished = false;
        loop {
            if kill_at.is_some_and(|k| reports >= k) {
                break;
            }
            let Some(msg) = writer.recv() else {
                panic!("{ctx}: the writer exited without `done`");
            };
            reports += 1;
            model.record(&msg, &ctx);
            if msg == "done" {
                finished = true;
                break;
            }
            reader.step(
                &mut rng,
                &mut model,
                stats,
                &format!("{ctx} report {reports} ({msg})"),
            );
        }
        if finished {
            writer.finish();
        } else {
            for msg in writer.kill() {
                model.record(&msg, &ctx);
            }
            stats.kills += 1;
        }
        // The writer is gone (killed or done): the reader catches up with the files as it left them.
        reader.catch_up(&mut model, stats, &format!("{ctx} after it stopped"));
        reader.settle(&model);
        verify_ok(
            &[],
            &path,
            &format!("{ctx}: verify after the writer stopped"),
        );
    }

    // Invariant 4 (no blob missing: the reader lease holds GC back), invariant 5 (no phantoms).
    assert!(
        reader.missing.is_empty(),
        "seed {seed}: blobs not found {:?}",
        reader.missing
    );
    for id in &reader.shown_ids {
        assert!(
            model.put_started.contains(id),
            "seed {seed}: phantom document {id}"
        );
    }
    // Invariant 6: the files as the last writer left them.
    verify_ok(
        &["--deep"],
        &path,
        &format!("seed {seed}: verify --deep at the end"),
    );
    let fresh = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default()).unwrap();
    let snapshot = fresh.database().snapshot();
    let ids = live_ids(&snapshot, COLLECTION);
    let c = snapshot.collection(COLLECTION).unwrap();
    for id in &ids {
        let mut out = Vec::new();
        fresh
            .get_blob(c.get(id).unwrap().unwrap().blob_ref().unwrap())
            .unwrap_or_else(|e| panic!("seed {seed}: get_blob({id}) at the end: {e}"))
            .read_to_end(&mut out)
            .unwrap();
        assert!(
            out == bytes_of(id),
            "seed {seed}: {id}'s blob differs at the end"
        );
    }
    reader.open(&model, stats, "the end");
    reader.refresh(true, &mut model, stats, "the end");
    reader.settle(&model);
    assert_eq!(
        reader.last_ids, ids,
        "seed {seed}: the reader and a fresh open disagree"
    );
    assert!(
        !reader.missing.iter().any(|id| ids.contains(id)),
        "seed {seed}: a live document's blob was not found"
    );
    reader.io.finish();
    stats.rewrites += model.rewrites;
    eprintln!(
        "seed {seed}: {} documents live at the end, {} puts, {} deletes, {} commits, {} GCs ({} rewrites, {} blobs swept) in {:.1?}",
        ids.len(),
        model.put_started.len(),
        model.del_acked.len(),
        model.commits,
        model.gcs,
        model.rewrites,
        model.swept,
        started.elapsed()
    );
}

#[test]
fn a_reader_refreshes_beside_a_gc_writer_that_is_killed_and_recovers() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x5eed_9a11_4e7f);
    let files = env_count("PERSIA_TEST_FILES", FILES);
    let kills = env_count("PERSIA_TEST_KILLS", KILLS_PER_FILE);
    let ops = env_count("PERSIA_TEST_OPS", OPS);
    let mut stats = Stats::default();
    let started = Instant::now();
    for file in 0..files {
        run_file(seed.wrapping_add(file), kills, ops, &mut stats);
    }
    stats.refresh_us.sort_unstable();
    eprintln!(
        "{} opens, {} refreshes taken ({} from an older open in the same state, {} found new blobs), {} refused as older, \
         {} blobs in the GC window, {} rewrites, {} kills, {} MiB read; refresh p50 {} us, p99 {} us, max {} us; {:.1?}",
        stats.opens,
        stats.taken,
        stats.taken_from_older,
        stats.found_new,
        stats.refused_older,
        stats.missing_window,
        stats.rewrites,
        stats.kills,
        stats.bytes_read >> 20,
        percentile(&stats.refresh_us, 50),
        percentile(&stats.refresh_us, 99),
        stats.refresh_us.last().copied().unwrap_or(0),
        started.elapsed()
    );
    assert!(stats.taken > 0 && stats.kills == files * kills);
    // A seed may rewrite no pack and refuse no older open. Those two are forced by the stepped test below; the
    // counts here are an observation.
    eprintln!(
        "seed {seed}: schedule observation, {} RefreshedOlder refusals and {} pack rewrites",
        stats.refused_older, stats.rewrites
    );
}

/// Whether the file system under `dir` lets the writer judge a reader lease by its lock (`locks_per_handle`, ADR-0054
/// item 13); where it does not, every lease holds GC back, and the reason is printed.
fn locks_judged(dir: &Path) -> bool {
    let probe = dir.join("locks-probe");
    let file = std::fs::File::create(&probe).unwrap();
    let judged = ContainerFile::locks_per_handle(&file);
    drop(file);
    std::fs::remove_file(&probe).unwrap();
    if !judged {
        eprintln!(
            "skipped: locks cannot be judged on the file system under {}",
            dir.display()
        );
    }
    judged
}

/// Invariant 4, step by step across processes: the reader holds an open that shows `gone` and `kept`; the writer
/// deletes `gone` and commits, and its GC is held back by the reader's lease (it deletes nothing, no rewrite); the
/// refresh with the held open is taken and shows `gone`, whose blob still reads. Once the reader caught up (it no
/// longer shows `gone`), the next GC sweeps `gone`'s blob and rewrites the pack, and nothing is missing. An open held across the next commit is then `RefreshedOlder`. Then the writer is killed (SIGKILL)
/// between a put and its commit, and the reader's refresh is taken.
#[test]
fn a_held_open_holds_gc_back_until_the_reader_catches_up() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    let mut model = Model::default();
    let mut stats = Stats::default();
    let mut writer = ChildIo::spawn("stepped-writer", &path, 0, "", 0);
    assert_eq!(writer.expect("writer"), "opened");
    let step = |writer: &mut ChildIo, model: &mut Model, line: &str| {
        writer.send(line);
        let reply = writer.expect(line);
        assert!(reply.starts_with(&format!("ok {line}")), "{line}: {reply}");
        match line.split_once(' ') {
            Some(("put", id)) => {
                model.put_started.insert(id.to_owned());
                model.put_written.insert(id.to_owned());
            }
            Some(("del", id)) => {
                model.del_started.insert(id.to_owned());
                model.del_acked.insert(id.to_owned());
            }
            _ => {}
        }
        reply
    };
    for line in ["put gone", "put kept", "commit"] {
        step(&mut writer, &mut model, line);
    }
    let mut reader = Reader::spawn(&path);
    reader.catch_up(&mut model, &mut stats, "before the delete");
    assert_eq!(
        reader.last_ids,
        BTreeSet::from(["gone".to_owned(), "kept".to_owned()])
    );
    reader.open(&model, &mut stats, "held");
    for line in ["del gone", "commit"] {
        step(&mut writer, &mut model, line);
    }
    assert_eq!(
        step(&mut writer, &mut model, "gc"),
        "ok gc 0 false",
        "held back by the reader's lease"
    );
    reader.refresh(true, &mut model, &mut stats, "the held open");
    assert!(reader.missing.is_empty(), "{:?}", reader.missing);
    assert!(reader.last_ids.contains("gone"));
    assert_eq!(stats.missing_window, 0);
    reader.catch_up(&mut model, &mut stats, "after the held-back gc");
    assert_eq!(reader.last_ids, BTreeSet::from(["kept".to_owned()]));
    assert!(reader.missing.is_empty());
    let gc = step(&mut writer, &mut model, "gc");
    // Where the writer cannot judge leases by their lock, every lease holds GC back (ADR-0054 item 13).
    if locks_judged(dir.path()) {
        assert_eq!(gc, "ok gc 1 true");
    }

    // The free-running schedule may never refuse an older open or rewrite the pack. This does both: the second `gc`
    // above reported a rewrite, and this open was taken before `newer` was committed, so catching up refuses it.
    reader.open(&model, &mut stats, "held before the later commit");
    for line in ["put newer", "commit"] {
        step(&mut writer, &mut model, line);
    }
    let refused_before = stats.refused_older;
    reader.catch_up(&mut model, &mut stats, "after the later commit");
    assert_eq!(
        stats.refused_older,
        refused_before + 1,
        "the open held before the commit was not refused as older"
    );
    assert_eq!(
        reader.last_ids,
        BTreeSet::from(["kept".to_owned(), "newer".to_owned()])
    );

    step(&mut writer, &mut model, "put after");
    drop(writer.kill());
    reader.catch_up(&mut model, &mut stats, "after the kill");
    assert_eq!(
        reader.last_ids,
        BTreeSet::from(["after".to_owned(), "kept".to_owned(), "newer".to_owned()])
    );
    reader.settle(&model);
    assert!(reader.missing.is_empty());
    verify_ok(&["--deep"], &path, "after the kill");
    reader.io.finish();
}

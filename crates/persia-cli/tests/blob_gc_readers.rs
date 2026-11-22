//! GC on the local pack beside read-only readers in other processes, and beside `persia verify` (SPEC §9.3 "Reader
//! leases", §9.5; ROADMAP 9.2.16; ADR-0054). The children are this test binary re-executed (`blob_support`); they
//! report each step on stdout and wait on stdin, so no test sleeps.
//!
//! **Readers** (`gc_holds_back_for_readers_in_other_processes_and_sweeps_once_they_catch_up_or_die`): one writer
//! process and `READERS` reader processes (`ReadOnlyBlobDatabase`) on one database. The writer runs seeded batches of
//! `put`, `delete`, `commit` and `gc` (a forced rewrite) freely; while a batch runs, the parent sends seeded readers
//! `open` (a read-only open of the documents, held), `refresh` (`ReadOnlyBlobDatabase::refresh` with the held open,
//! taken earlier, or a new one) or `read` (no refresh), and each reader reads every blob its documents show, whole,
//! and checks it byte for byte. A refresh with a held open shows documents older than the pack it reopens: the
//! window a GC that ignored the reader would turn into `BlobNotFound`. At seeded points a reader is killed (SIGKILL),
//! idle or right after a `refresh` was sent (mid-refresh), and a new one takes its place. Between batches the writer
//! commits, every live reader catches up (some are killed instead), and the writer collects again; then it deletes
//! documents without committing, collects, and every reader reads again. No reader keeps a snapshot across a refresh
//! (the documented exception, ADR-0054 item 4).
//!
//! Invariants:
//! 1. No reader ever gets `BlobNotFound`, or any other error or other bytes, for a blob its current documents
//!    reference: GC is held back while any reader is behind the committed seq (or opening).
//! 2. A reader's lease record names its documents' seq after every refresh (bytes 48..56, SPEC §9.3).
//! 3. A GC that was not held back saw every live lease current. Once every live reader is caught up (or dead), the
//!    first GC sweeps (it is not held back, all live leases are current), and after a commit the pack then holds
//!    exactly the blobs of the live documents. A GC that deletes documents without committing still sweeps, and the
//!    readers still read those documents' blobs.
//! 4. A killed reader's lease is stale: every one is removed by the next GC (the reports' `stale_removed` add up to
//!    the kills), the leases directory holds exactly the live readers' leases after a caught-up GC, and none once
//!    every reader has gone.
//!
//! **Verify** (`persia_verify_beside_a_gc_writer_never_reports_a_dangling_reference`): a writer process churns
//! (`put`, `delete`, `commit`, `gc` with a forced rewrite) while `persia verify` runs again and again (`--quick`,
//! default, `--deep`). `persia verify` takes a reader lease before it opens the documents, so it never sees a
//! dangling reference: never exit 3, never "changed during verify"; every run succeeds.
//!
//! Both are skipped (with the reason printed) where locks cannot be judged (`locks_per_handle`, ADR-0054 item 13):
//! there a killed reader holds GC back for good, by design.
//!
//! Seeds: `PERSIA_TEST_SEED` (printed) sets the schedules; `PERSIA_TEST_FILES`, `PERSIA_TEST_ROUNDS` and
//! `PERSIA_TEST_VERIFIES` override the databases, the rounds per database and the verify runs. A replay reproduces the
//! schedules (operations, command points, kills), not the exact instants. On a failure the temp dir is kept and its
//! path printed.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::too_many_lines
)] // Tests: inputs are generated or fixed.

mod blob_support;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use blob_support::{
    COLLECTION, ChildIo, KeptOnFailure, Rng, base_seed, child_env, content, env_count, hash_str,
    live_ids, parent_gone, persia, pick_size, report, serial, verify_ok,
};
use persia::internal::documents::{Blob, BlobDatabase, Error, ReadOnlyBlobDatabase};
use persia::internal::gc::{GcOptions, GcReport};
use persia_engine::db::{DatabaseOptions, ReadOnlyDatabase};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::container::ContainerFile;

#[cfg(not(feature = "slow"))]
const FILES: u64 = 1;
#[cfg(feature = "slow")]
const FILES: u64 = 4;

#[cfg(not(feature = "slow"))]
const ROUNDS: u64 = 4;
#[cfg(feature = "slow")]
const ROUNDS: u64 = 12;

/// Reader processes alive at once.
#[cfg(not(feature = "slow"))]
const READERS: usize = 3;
#[cfg(feature = "slow")]
const READERS: usize = 5;

/// Writer operations per batch.
const OPS: u64 = 24;

#[cfg(not(feature = "slow"))]
const VERIFIES: u64 = 24;
#[cfg(feature = "slow")]
const VERIFIES: u64 = 120;

/// The share of blobs at or above the 4 MiB chunk: low, every reader reads every blob after every command.
const BIG_PERCENT: u64 = 1;

/// The blob of document `id`: a pure function of the id, as every process derives it.
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
        "batch-writer" => batch_writer(&env.path, &env.prefix),
        "reader" => reader(&env.path),
        "churn-writer" => churn_writer(&env.path, env.seed, &env.prefix),
        other => panic!("unknown child role {other}"),
    }
}

/// `gc <key=value ...>`: what one GC did, and its reader lease counts (`leases=none` without a pack).
fn gc_line(gc: &GcReport) -> String {
    let leases = match &gc.readers {
        Some(r) => format!(
            " held={} live={} current={} behind={} opening={} ahead={} unreadable={} stale={}",
            r.held_back,
            r.live,
            r.current,
            r.behind,
            r.opening,
            r.ahead,
            r.unreadable,
            r.stale_removed
        ),
        None => " held=false live=0 current=0 stale=0 leases=none".to_owned(),
    };
    format!(
        "gc deleted={} rewrite={}{leases}",
        gc.sweep.blobs_deleted,
        format!("{:?}", gc.sweep.rewrite).contains("Done"),
    )
}

/// A forced-rewrite GC of `$db`: a macro, as the pack store type of `BlobDatabase::open` lives in `persia-blob`, which
/// this crate does not depend on.
macro_rules! forced_gc {
    ($db:expr) => {
        $db.gc(&GcOptions {
            rewrite_ratio: 0.0,
            ..GcOptions::default()
        })
        .unwrap()
    };
}

/// Puts document `$id` with its blob (`bytes_of`) at `$durability`.
macro_rules! put {
    ($db:expr, $id:expr, $durability:expr) => {
        $db.put(
            COLLECTION,
            &Document::new($id),
            Blob::from_reader(&bytes_of($id)[..]),
            $durability,
        )
        .unwrap()
    };
}

/// Deletes document `$id` (`Fsync`) and releases its pins.
macro_rules! delete {
    ($db:expr, $id:expr) => {{
        $db.log_mut()
            .delete(COLLECTION, $id, Durability::Fsync)
            .unwrap();
        $db.release_durable();
    }};
}

/// The writer of the readers scenario. Takes one command per stdin line:
/// - `run <n> <seed>`: `n` seeded operations, freely: `put <prefix>-<k>` (reports `put <id>`), `del` of a live
///   document (`del <id>`), `commit` (`commit <seq>`) or a forced `gc` (`gc ...`, [`gc_line`]); then `ran <seq>`;
/// - `commit` (`commit <seq>`), `gc` (`gc ...`);
/// - `delnc <n> <seed>`: deletes `n` live documents without committing (`del <id>` each), then `ok`;
/// - `check`: `check pack=<blobs in the pack> live=<blobs of live documents> extra=<in the pack, not live>
///   lost=<live, not in the pack>`.
fn batch_writer(path: &Path, prefix: &str) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    let mut live_docs: Vec<String> = live_ids(&db.log().snapshot(), COLLECTION)
        .into_iter()
        .collect();
    let mut next = 0_u64;
    report("opened");
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let words: Vec<&str> = line.split(' ').collect();
        match words.as_slice() {
            ["run", n, seed] => {
                let mut rng = Rng(seed.parse().unwrap());
                for _ in 0..n.parse::<u64>().unwrap() {
                    let roll = rng.range(0, 100);
                    if roll < 50 || live_docs.is_empty() {
                        let id = format!("{prefix}-{next}");
                        next += 1;
                        let durability = if rng.chance(30) {
                            Durability::Fsync
                        } else {
                            Durability::Async
                        };
                        put!(db, &id, durability);
                        report(&format!("put {id}"));
                        live_docs.push(id);
                    } else if roll < 70 {
                        let i = usize::try_from(rng.range(0, live_docs.len() as u64)).unwrap();
                        let id = live_docs.swap_remove(i);
                        delete!(db, &id);
                        report(&format!("del {id}"));
                    } else if roll < 85 {
                        let seq = db.commit().unwrap();
                        report(&format!("commit {seq}"));
                    } else {
                        report(&gc_line(&forced_gc!(db)));
                    }
                }
                report(&format!("ran {}", db.log().seq()));
            }
            ["commit"] => {
                let seq = db.commit().unwrap();
                report(&format!("commit {seq}"));
            }
            ["gc"] => report(&gc_line(&forced_gc!(db))),
            ["delnc", n, seed] => {
                let mut rng = Rng(seed.parse().unwrap());
                for _ in 0..n.parse::<u64>().unwrap() {
                    if live_docs.is_empty() {
                        break;
                    }
                    let i = usize::try_from(rng.range(0, live_docs.len() as u64)).unwrap();
                    let id = live_docs.swap_remove(i);
                    delete!(db, &id);
                    report(&format!("del {id}"));
                }
                report("ok");
            }
            ["check"] => {
                let snapshot = db.log().snapshot();
                let c = snapshot.collection(COLLECTION).unwrap();
                let referenced: BTreeSet<String> = live_ids(&snapshot, COLLECTION)
                    .iter()
                    .map(|id| {
                        c.get(id)
                            .unwrap()
                            .unwrap()
                            .blob_ref()
                            .unwrap()
                            .blob_id()
                            .to_string()
                    })
                    .collect();
                let pack: BTreeSet<String> = db
                    .blobs()
                    .store()
                    .blob_ids()
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                report(&format!(
                    "check pack={} live={} extra={} lost={}",
                    pack.len(),
                    referenced.len(),
                    pack.difference(&referenced).count(),
                    referenced.difference(&pack).count(),
                ));
            }
            _ => panic!("batch writer: unknown command {line}"),
        }
    }
}

/// Every blob `ro`'s documents reference, read whole and checked: `<seq> docs=<n> bytes=<n> missing=<ids> bad=<what>`.
/// `missing` lists `BlobNotFound`; `bad` any other error, other bytes, or a document without a blob.
fn read_all(ro: &ReadOnlyBlobDatabase) -> String {
    let snapshot = ro.database().snapshot();
    let c = snapshot.collection(COLLECTION).unwrap();
    let ids = live_ids(&snapshot, COLLECTION);
    let (mut missing, mut bad, mut bytes) = (Vec::new(), Vec::new(), 0_usize);
    for id in &ids {
        let doc = c.get(id).unwrap().unwrap();
        let Some(blob) = doc.blob_ref() else {
            bad.push(format!("{id}:no-blob-ref"));
            continue;
        };
        let got = ro.get_blob(blob).and_then(|mut r| {
            let mut out = Vec::new();
            r.read_to_end(&mut out).map_err(|source| Error::Open {
                path: id.into(),
                source,
            })?;
            Ok(out)
        });
        match got {
            Ok(out) if out == bytes_of(id) => bytes += out.len(),
            Ok(_) => bad.push(format!("{id}:other-bytes")),
            Err(Error::Blob(e)) if format!("{e:?}").starts_with("BlobNotFound") => {
                missing.push(id.clone());
            }
            Err(e) => bad.push(format!("{id}:{}", format!("{e:?}").replace(' ', "_"))),
        }
    }
    format!(
        "{} docs={} bytes={bytes} missing={} bad={}",
        ro.database().seq(),
        ids.len(),
        missing.join(","),
        bad.join(","),
    )
}

/// A reader: opens read-only and reports `ready <lease file> <seq>`, then takes commands on stdin:
/// - `open`: a new read-only open of the documents, held (replacing one held before); reports `held <seq>`;
/// - `refresh held`: refreshes the handle with the held open (taken earlier: the writer may have committed, deleted
///   and collected since, so these documents are older than the pack the refresh reopens), or a new one if none is
///   held; `refresh new`: drops any held open and refreshes with a new one. Reports `refreshed <read_all>`, or
///   `refresh-error <e>`;
/// - `read`: `read <read_all>`, without a refresh.
///
/// A held open is always taken after the handle's documents, so its refresh is never `RefreshedOlder`. The reader
/// never keeps a snapshot across a refresh.
fn reader(path: &Path) {
    let options = DatabaseOptions::default();
    let mut ro = ReadOnlyBlobDatabase::open(path, &options).unwrap();
    let lease = ro
        .lease()
        .expect("the reader holds a lease")
        .path()
        .unwrap()
        .to_owned();
    let mut held: Option<ReadOnlyDatabase> = None;
    report(&format!(
        "ready {} {}",
        lease.display(),
        ro.database().seq()
    ));
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        match line.as_str() {
            "open" => {
                let db = ReadOnlyDatabase::open(path, &options.registry).unwrap();
                report(&format!("held {}", db.seq()));
                held = Some(db);
            }
            "refresh held" | "refresh new" => {
                if line == "refresh new" {
                    held = None;
                }
                let reopened = match held.take() {
                    Some(db) => db,
                    None => ReadOnlyDatabase::open(path, &options.registry).unwrap(),
                };
                match ro.refresh(reopened) {
                    Ok(_) => report(&format!("refreshed {}", read_all(&ro))),
                    Err(e) => report(&format!("refresh-error {e:?}").replace('\n', " ")),
                }
            }
            "read" => report(&format!("read {}", read_all(&ro))),
            other => panic!("reader: unknown command {other}"),
        }
    }
}

/// The writer of the verify scenario: seeded `put`, `delete`, `commit` and forced `gc` until stdin closes; reports
/// `opened`, `op <n>` every 8 operations, then `stats gcs=<n> held=<n> swept=<n> deleted=<n>`.
fn churn_writer(path: &Path, seed: u64, prefix: &str) {
    let gone = parent_gone();
    let mut rng = Rng(seed);
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    let mut live: Vec<String> = Vec::new();
    let (mut gcs, mut held, mut swept, mut deleted) = (0_u64, 0_u64, 0_u64, 0_u64);
    report("opened");
    for n in 0_u64.. {
        if gone.load(Ordering::Relaxed) {
            break;
        }
        let roll = rng.range(0, 100);
        if roll < 45 || live.is_empty() {
            let id = format!("{prefix}-{n}");
            put!(db, &id, Durability::Async);
            live.push(id);
        } else if roll < 70 {
            let i = usize::try_from(rng.range(0, live.len() as u64)).unwrap();
            let id = live.swap_remove(i);
            delete!(db, &id);
        } else if roll < 78 {
            db.commit().unwrap();
        } else {
            let gc = forced_gc!(db);
            gcs += 1;
            if gc.readers.as_ref().is_some_and(|r| r.held_back) {
                held += 1;
            } else {
                swept += 1;
                deleted += gc.sweep.blobs_deleted;
            }
        }
        if n % 8 == 7 {
            report(&format!("op {n}"));
        }
    }
    report(&format!(
        "stats gcs={gcs} held={held} swept={swept} deleted={deleted}"
    ));
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

/// Whether the file system under `dir` lets the writer judge a lease by its lock (ADR-0054 item 13); printed when not.
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

/// The `key=value` words of a report line.
fn fields(line: &str) -> BTreeMap<&str, &str> {
    line.split(' ').filter_map(|w| w.split_once('=')).collect()
}

fn num(fields: &BTreeMap<&str, &str>, key: &str) -> u64 {
    fields
        .get(key)
        .unwrap_or_else(|| panic!("no {key} in {fields:?}"))
        .parse()
        .unwrap()
}

/// The `manifest_seq` of the lease record at `path` (SPEC §9.3: bytes 48..56, little-endian), and whether it is open
/// (a non-zero `db_uuid`, bytes 16..32).
fn lease_seq(path: &Path) -> (bool, u64) {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(bytes.len(), 64, "{}", path.display());
    let open = bytes[16..32].iter().any(|&b| b != 0);
    (open, u64::from_le_bytes(bytes[48..56].try_into().unwrap()))
}

/// The lease files in the leases directory of the database at `path`.
fn lease_files(path: &Path) -> BTreeSet<PathBuf> {
    match std::fs::read_dir(path.with_extension("persia.blobs.leases")) {
        Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// One reader process and the lease file it reported.
struct Reader {
    io: ChildIo,
    lease: PathBuf,
}

#[derive(Debug, Default)]
struct Stats {
    reads: u64,
    refreshes: u64,
    held_opens: u64,
    bytes: u64,
    kills: u64,
    mid_refresh_kills: u64,
    gcs: u64,
    held: u64,
    swept: u64,
    deleted: u64,
    stale: u64,
    caught_up_gcs: u64,
}

/// The parent's side of one run: the writer, the readers, and what was seen.
struct Run {
    path: PathBuf,
    seed: u64,
    rng: Rng,
    writer: ChildIo,
    readers: Vec<Reader>,
    spawned: u64,
    stats: Stats,
}

impl Run {
    fn spawn_reader(&mut self) {
        let name = format!("r{}", self.spawned);
        self.spawned += 1;
        let io = ChildIo::spawn("reader", &self.path, self.seed, &name, 0);
        let ready = io.expect("reader ready");
        let rest = ready
            .strip_prefix("ready ")
            .unwrap_or_else(|| panic!("seed {}: {name}: {ready}", self.seed));
        let (lease, _seq) = rest.rsplit_once(' ').unwrap();
        self.readers.push(Reader {
            io,
            lease: PathBuf::from(lease),
        });
    }

    /// Checks a reader's `refreshed` / `read` reply (invariants 1 and 2).
    fn check_read(&mut self, i: usize, cmd: &str, reply: &str, ctx: &str) {
        let seed = self.seed;
        let name = &self.readers[i].io.name;
        let refresh = cmd.starts_with("refresh");
        let want = if refresh { "refreshed " } else { "read " };
        let body = reply
            .strip_prefix(want)
            .unwrap_or_else(|| panic!("seed {seed}: {ctx}: {name} {cmd}: {reply}"));
        let f = fields(body);
        assert!(
            f["missing"].is_empty() && f["bad"].is_empty(),
            "seed {seed}: {ctx}: {name} {cmd}: {reply}"
        );
        let seq: u64 = body.split(' ').next().unwrap().parse().unwrap();
        assert_eq!(
            lease_seq(&self.readers[i].lease),
            (true, seq),
            "seed {seed}: {ctx}: {name}'s lease record after {cmd}"
        );
        self.stats.bytes += num(&f, "bytes");
        if refresh {
            self.stats.refreshes += 1;
        } else {
            self.stats.reads += 1;
        }
    }

    /// Sends `cmd` to reader `i` and checks its reply.
    fn command(&mut self, i: usize, cmd: &str, ctx: &str) {
        self.readers[i].io.send(cmd);
        let reply = self.readers[i].io.expect(ctx);
        if cmd == "open" {
            assert!(
                reply.starts_with("held "),
                "seed {}: {ctx}: {} open: {reply}",
                self.seed,
                self.readers[i].io.name
            );
            self.stats.held_opens += 1;
            return;
        }
        self.check_read(i, cmd, &reply, ctx);
    }

    /// SIGKILLs reader `i` (idle, or right after a `refresh` was sent) and starts another in its place. The writer
    /// runs meanwhile, so its GC may already have removed the stale lease: that is checked at the next catch-up.
    fn kill_reader(&mut self, i: usize, mid_refresh: bool) {
        let mut reader = self.readers.swap_remove(i);
        if mid_refresh {
            reader.io.send("refresh held");
            self.stats.mid_refresh_kills += 1;
        }
        reader.io.kill();
        self.stats.kills += 1;
        self.spawn_reader();
    }

    /// Records a writer `gc` line; checks invariant 3's first half: a sweep that ran saw every live lease current.
    fn gc_seen(&mut self, line: &str, ctx: &str) -> BTreeMap<String, String> {
        let f: BTreeMap<String, String> = fields(line)
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let get = |k: &str| -> u64 { f[k].parse().unwrap() };
        self.stats.gcs += 1;
        self.stats.stale += get("stale");
        if f["held"] == "true" {
            self.stats.held += 1;
            assert_eq!(get("deleted"), 0, "seed {}: {ctx}: {line}", self.seed);
        } else {
            self.stats.swept += 1;
            self.stats.deleted += get("deleted");
            assert_eq!(
                get("live"),
                get("current"),
                "seed {}: {ctx}: a sweep beside a lease that is not current: {line}",
                self.seed
            );
        }
        f
    }

    /// The writer's next report, which must start with `want`.
    fn writer_expect(&self, want: &str, ctx: &str) -> String {
        let line = self.writer.expect(ctx);
        assert!(
            line.starts_with(want),
            "seed {}: {ctx}: expected {want}, got {line}",
            self.seed
        );
        line
    }

    /// One batch: the writer runs freely while seeded readers refresh, read, or are killed.
    fn batch(&mut self, round: u64) {
        let ctx = format!("round {round} batch");
        let batch_seed = self.rng.next_u64();
        self.writer.send(&format!("run {OPS} {batch_seed}"));
        loop {
            let line = self.writer.expect(&ctx);
            if line.starts_with("ran ") {
                break;
            }
            if line.starts_with("gc ") {
                self.gc_seen(&line, &ctx);
            }
            if !self.rng.chance(40) {
                continue;
            }
            let i = usize::try_from(self.rng.range(0, self.readers.len() as u64)).unwrap();
            match self.rng.range(0, 100) {
                0..=4 => self.kill_reader(i, false),
                5..=9 => self.kill_reader(i, true),
                10..=29 => self.command(i, "open", &ctx),
                30..=64 => self.command(i, "refresh held", &ctx),
                _ => self.command(i, "read", &ctx),
            }
        }
    }

    /// Invariants 3 and 4 between batches: commit; every reader catches up or is killed; the GC sweeps and leaves
    /// the pack holding exactly the live documents' blobs; the leases directory holds exactly the live leases. Then
    /// deletes without a commit, a GC that still sweeps, and every reader reads its documents' blobs again.
    fn catch_up(&mut self, round: u64) {
        let ctx = format!("round {round} catch-up");
        self.writer.send("commit");
        self.writer_expect("commit ", &ctx);
        if self.rng.chance(40) {
            let i = usize::try_from(self.rng.range(0, self.readers.len() as u64)).unwrap();
            let mut dead = self.readers.swap_remove(i);
            dead.io.send("refresh held");
            let lease = dead.lease.clone();
            dead.io.kill();
            // The writer is idle: nothing removed it yet.
            assert!(
                lease.exists(),
                "seed {}: {ctx}: a killed reader never removes its lease",
                self.seed
            );
            self.stats.kills += 1;
            self.stats.mid_refresh_kills += 1;
        }
        for i in 0..self.readers.len() {
            self.command(i, "refresh new", &ctx);
        }
        self.writer.send("gc");
        let line = self.writer_expect("gc ", &ctx);
        let f = self.gc_seen(&line, &ctx);
        self.stats.caught_up_gcs += 1;
        let readers = self.readers.len() as u64;
        assert_eq!(
            (f["held"].as_str(), f["live"].parse::<u64>().unwrap()),
            ("false", readers),
            "seed {}: {ctx}: every reader is caught up or dead: {line}",
            self.seed
        );
        // At least the kills: a reader that started during a batch may also have lost its fresh, not yet locked file
        // to a GC (ADR-0054 item 4: it then starts again under a new id).
        assert!(
            self.stats.stale >= self.stats.kills,
            "seed {}: {ctx}: every killed reader's lease is removed by the next GC: {} removed, {} kills",
            self.seed,
            self.stats.stale,
            self.stats.kills
        );
        // macOS: the child reports the canonical lease path (`/private/var/...`) while a directory
        // listing of the temp dir is `/var/...`. Compare the real paths, exactly.
        let leases: BTreeSet<PathBuf> = self
            .readers
            .iter()
            .map(|r| std::fs::canonicalize(&r.lease).unwrap())
            .collect();
        let listed: BTreeSet<PathBuf> = lease_files(&self.path)
            .into_iter()
            .map(|p| std::fs::canonicalize(p).unwrap())
            .collect();
        assert_eq!(listed, leases, "seed {}: {ctx}", self.seed);
        self.writer.send("check");
        let check = self.writer_expect("check ", &ctx);
        let c = fields(&check);
        assert_eq!(
            (num(&c, "extra"), num(&c, "lost")),
            (0, 0),
            "seed {}: {ctx}: after a caught-up GC the pack holds exactly the live blobs: {check}",
            self.seed
        );

        // Deleted without a commit: the readers are still current, so GC sweeps, and keeps what they show.
        let delnc_seed = self.rng.next_u64();
        self.writer.send(&format!("delnc 3 {delnc_seed}"));
        while self.writer.expect(&ctx) != "ok" {}
        self.writer.send("gc");
        let line = self.writer_expect("gc ", &ctx);
        let f = self.gc_seen(&line, &ctx);
        assert_eq!(f["held"], "false", "seed {}: {ctx}: {line}", self.seed);
        for i in 0..self.readers.len() {
            self.command(i, "read", &ctx);
        }
        // Whatever the stale ones were, a reader replaces each one killed here.
        while self.readers.len() < READERS {
            self.spawn_reader();
        }
    }
}

fn run_readers(seed: u64, rounds: u64, stats: &mut Stats) {
    let dir = KeptOnFailure::new();
    if !locks_judged(dir.path()) {
        return;
    }
    // `/var` on macOS is a symlink to `/private/var`. Open under the canonical directory so the
    // lease paths the children report match a listing of that directory.
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = create(&root);
    let writer = ChildIo::spawn("batch-writer", &path, seed, "w", 0);
    assert_eq!(writer.expect("writer open"), "opened");
    let mut run = Run {
        path,
        seed,
        rng: Rng(seed),
        writer,
        readers: Vec::new(),
        spawned: 0,
        stats: Stats::default(),
    };
    for _ in 0..READERS {
        run.spawn_reader();
    }
    for round in 0..rounds {
        run.batch(round);
        run.catch_up(round);
    }

    // Every reader gone: the last GC removes nothing (no reader was killed since the last catch-up) and sweeps.
    let leases: Vec<PathBuf> = run.readers.iter().map(|r| r.lease.clone()).collect();
    for reader in std::mem::take(&mut run.readers) {
        reader.io.finish();
    }
    for lease in &leases {
        assert!(
            !lease.exists(),
            "seed {seed}: a reader that exited keeps its lease"
        );
    }
    assert!(lease_files(&run.path).is_empty(), "seed {seed}");
    run.writer.send("gc");
    let line = run.writer_expect("gc ", "the last gc");
    let f = run.gc_seen(&line, "the last gc");
    assert_eq!(
        (f["held"].as_str(), f["live"].as_str()),
        ("false", "0"),
        "seed {seed}: {line}"
    );
    assert!(run.stats.stale >= run.stats.kills, "seed {seed}");
    let Run {
        path,
        writer,
        stats: s,
        ..
    } = run;
    writer.finish();
    verify_ok(&["--deep"], &path, &format!("seed {seed}: at the end"));
    eprintln!("seed {seed}: {s:?}");
    assert!(
        s.caught_up_gcs == rounds && s.swept >= rounds,
        "seed {seed}: {s:?}"
    );
    stats.reads += s.reads;
    stats.refreshes += s.refreshes;
    stats.held_opens += s.held_opens;
    stats.bytes += s.bytes;
    stats.kills += s.kills;
    stats.mid_refresh_kills += s.mid_refresh_kills;
    stats.gcs += s.gcs;
    stats.held += s.held;
    stats.swept += s.swept;
    stats.deleted += s.deleted;
    stats.stale += s.stale;
    stats.caught_up_gcs += s.caught_up_gcs;
}

#[test]
fn gc_holds_back_for_readers_in_other_processes_and_sweeps_once_they_catch_up_or_die() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let base = base_seed(0x6c5e_a5e5_0001);
    let files = env_count("PERSIA_TEST_FILES", FILES);
    let rounds = env_count("PERSIA_TEST_ROUNDS", ROUNDS);
    eprintln!("READERS={READERS} OPS={OPS} BIG_PERCENT={BIG_PERCENT}");
    let started = Instant::now();
    let mut stats = Stats::default();
    for file in 0..files {
        run_readers(base.wrapping_add(file), rounds, &mut stats);
    }
    eprintln!(
        "{files} databases x {rounds} rounds in {:?}: {stats:?}",
        started.elapsed()
    );
}

/// One `persia verify` run: its exit status and stderr.
fn verify_once(args: &[&str], path: &Path) -> (Option<i32>, String) {
    let mut all: Vec<&str> = vec!["verify"];
    all.extend_from_slice(args);
    all.push(path.to_str().unwrap());
    let out = persia(&all);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn persia_verify_beside_a_gc_writer_never_reports_a_dangling_reference() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x6c5e_a5e5_0002);
    let verifies = env_count("PERSIA_TEST_VERIFIES", VERIFIES);
    let dir = KeptOnFailure::new();
    if !locks_judged(dir.path()) {
        return;
    }
    let path = create(dir.path());
    let started = Instant::now();
    let writer = ChildIo::spawn("churn-writer", &path, seed, "v", 0);
    assert_eq!(writer.expect("churn writer"), "opened");
    // Let it build a pack first.
    for _ in 0..4 {
        assert!(writer.expect("churn writer").starts_with("op "));
    }
    let modes: [&[&str]; 3] = [&["--quick"], &[], &["--deep"]];
    let mut codes: BTreeMap<Option<i32>, u64> = BTreeMap::new();
    let mut failures = Vec::new();
    for n in 0..verifies {
        let args = modes[usize::try_from(n % 3).unwrap()];
        let (code, stderr) = verify_once(args, &path);
        *codes.entry(code).or_default() += 1;
        if code != Some(0) {
            failures.push(format!("run {n} {args:?}: exit {code:?}: {stderr}"));
        }
    }
    let rest = writer.finish();
    let stats = rest
        .iter()
        .find(|l| l.starts_with("stats "))
        .unwrap_or_else(|| panic!("seed {seed}: no stats in {rest:?}"))
        .clone();
    eprintln!(
        "seed {seed}: {verifies} verifies in {:?}: exit codes {codes:?}; writer {stats}",
        started.elapsed()
    );
    assert!(
        !codes.contains_key(&Some(3)),
        "seed {seed}: a verify reported damage: {failures:#?}"
    );
    assert!(failures.is_empty(), "seed {seed}: {failures:#?}");
    assert!(
        lease_files(&path).is_empty(),
        "seed {seed}: a verify left its lease"
    );
    verify_ok(&["--deep"], &path, &format!("seed {seed}: at the end"));
}

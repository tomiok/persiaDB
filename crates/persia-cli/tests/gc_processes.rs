//! `persia gc` as a real process (ROADMAP 10.4.5; SPEC §9.3, §9.5): killed mid-run, beside reader processes, beside
//! writer processes, and in a seeded soak. The children are the `persia` binary itself and this test binary
//! re-executed (`blob_support`), which report each step on stdout and wait on stdin, so no test sleeps.
//!
//! Invariants:
//! 1. **Crash safety** (`kill_9_of_persia_gc_keeps_every_referenced_blob_and_a_rerun_completes`): seeded rounds on
//!    one database. Each round, a writer (this process) replaces the big documents, stores and deletes a pure-garbage
//!    one, and puts, replaces and deletes small ones (sizes 0 B to 200 KiB and around the 4 MiB chunk, some content
//!    duplicated), commits, and sometimes leaves an uncommitted `Fsync` tail for `persia gc`'s recovery to replay.
//!    Then `persia gc` runs and is killed (SIGKILL) at a point the parent observes on the file system, with no
//!    timer, each kind in turn with seeded parameters: right after the spawn, after a seeded number of directory polls
//!    (the open, the mark, the lease scan), once the rewrite's temporary file `<pack>.tmp.*` appears, once it holds a
//!    seeded number of bytes (mid-copy or in its sync), or once the pack's inode changed (after the rename: the
//!    directory sync, the close); one run in six is not killed. Where each kill landed is printed per round, and the
//!    test fails unless some landed before any rewrite file and some mid-rewrite. After each kill:
//!    - `persia verify --deep` passes on the files as the kill left them: every reference bound, every frame's
//!      checksum right, every `BlobIndex` frame's chunks present and matching its manifest (so no manifest is left
//!      without its chunks), every referenced blob re-hashed against its id;
//!    - a pack whose inode did not change is a prefix of the pack before the run (the rewrite is never in place;
//!      the open may truncate the chunks a deduplicated put staged after `log_end`, SPEC §9.3); a replaced one holds
//!      exactly the referenced blobs, with no orphan chunk;
//!    - a read-only open shows exactly the documents of the model, each blob byte-identical to what was put;
//!    - a rerun of `persia gc` completes (status 0, not held back, no missing blob, `marked` equal to the blobs the
//!      references name), leaves no temporary file behind, and when it rewrote, the pack holds exactly the referenced
//!      blobs; `persia verify` and the reads pass again after it.
//! 2. **Reader processes** (SPEC §9.3 "Reader leases", §9.5): a reader process whose lease is behind the committed
//!    seq holds the sweep back (nothing deleted, the pack byte-identical, the report's reader counts say so) and keeps
//!    reading the blobs of documents the writer deleted; once it refreshes, `persia gc` sweeps and the reader reads on
//!    after the rewrite, and after a refresh that reopens the replaced pack. A reader at exactly the committed seq
//!    lets the sweep run and reads on the same way; a killed reader's lease is stale and the next run removes it.
//! 3. **Writer processes**: while a writer process holds the database, `persia gc` exits 1 and changes nothing;
//!    while a `persia gc` (stopped with `SIGSTOP` mid-rewrite) holds it, a writer process's open fails on the
//!    container's lock and a second `persia gc` exits 1, and neither changes a byte; once resumed, the first one
//!    completes.
//! 4. **Soak** (`seeded_soak_of_puts_deletes_and_replaces_interleaved_with_persia_gc`): seeded rounds of puts,
//!    replaces and deletes with blobs (`Fsync` and `Async`) across several commits, each followed by `persia gc`.
//!    After each run every live document's blob reads back byte-identical, the report marks exactly the blobs the
//!    references name and misses none, and either the pack was rewritten (it then holds exactly the referenced blobs,
//!    no orphan chunk, and `deleted` counts every unreferenced manifest the pack had) or the dead bytes were below the
//!    default ratio (nothing deleted, every manifest kept: by design, SPEC §9.3, so an unreferenced manifest survives
//!    only such a run).
//! 5. **A registered analyzer** (`kill_9_of_persia_gc_on_a_registered_analyzer_database_loses_no_blob`, ROADMAP
//!    10.4.7, ADR-0069): `persia gc` opens a database whose schema names an analyzer it lacks with the mark-only open,
//!    and is killed at each kind of point above: `verify --deep` passes, the application reads every blob, the one only
//!    an unflushed WAL record references included, that record is still a WAL record, and a rerun completes.
//!
//! Skipped (with the reason printed) where locks cannot be judged (`locks_per_handle`, ADR-0054 item 13): the reader
//! scenarios need a reader's lock to be judged.
//!
//! Seeds: `PERSIA_TEST_SEED` (printed) sets the schedules; `PERSIA_TEST_KILLS` and `PERSIA_TEST_ROUNDS` override the
//! crash rounds and the soak rounds. A replay reproduces the schedule (operations, sizes, kill points), not the exact
//! instant a kill lands. On a failure the temp dir is kept and its path printed.
#![cfg(unix)] // SIGKILL / SIGSTOP, `flock` locks, inode numbers.
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
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Instant;

use blob_support::{
    CHUNK, COLLECTION, ChildIo, KeptOnFailure, Rng, base_seed, child_env, content, env_count,
    hash_str, live_ids, persia, pick_size, report, row, serial, verify_ok, wait_for_eof,
};
use persia::internal::documents::{Blob, BlobDatabase, Error, ReadOnlyBlobDatabase};
use persia_engine::analysis::{Analyzer, AnalyzerRegistry, WhitespaceTokenizer};
use persia_engine::db::{DatabaseError, DatabaseOptions, ReadOnlyDatabase};
use persia_engine::document::{Document, Value};
use persia_engine::schema::{CollectionSchema, SchemaBuilder};
use persia_engine::wal::Durability;
use persia_storage::container::{ContainerFile, Error as ContainerError};
use serde_json::Value as Json;

#[cfg(not(feature = "slow"))]
const KILLS: u64 = 12;
#[cfg(feature = "slow")]
const KILLS: u64 = 48;

#[cfg(not(feature = "slow"))]
const SOAK_ROUNDS: u64 = 10;
#[cfg(feature = "slow")]
const SOAK_ROUNDS: u64 = 48;

/// Writer operations per soak round.
const SOAK_OPS: u64 = 30;

/// The `(size, content seed)` pairs some puts reuse: duplicates, deduplicated at bind, across rounds and runs.
const POOL: [(u64, u64); 3] = [(1000, 11), (70_000, 22), (1, 33)];

/// The live documents of a scenario: id to the `(size, content seed)` of its blob.
type Model = BTreeMap<String, (u64, u64)>;

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
        "reader" => reader(&env.path),
        "holder" => holder(&env.path),
        "try-writer" => try_writer(&env.path),
        other => panic!("unknown child role {other}"),
    }
}

/// The blob of reader-scenario document `id`: about 48 KiB, distinct per id, a pure function of it.
fn reader_bytes(id: &str) -> Vec<u8> {
    let h = hash_str(id);
    content((48 << 10) + h % 4096, h)
}

/// Every blob `ro`'s documents reference, read whole and checked: `<seq> docs=<n> bad=<what>`, where `bad` lists any
/// read error, other bytes, or a document without a blob (by id and the error's debug form, for the log only).
fn read_all(ro: &ReadOnlyBlobDatabase) -> String {
    let snapshot = ro.database().snapshot();
    let c = snapshot.collection(COLLECTION).unwrap();
    let ids = live_ids(&snapshot, COLLECTION);
    let mut bad = Vec::new();
    for id in &ids {
        let doc = c.get(id).unwrap().unwrap();
        let Some(blob) = doc.blob_ref() else {
            bad.push(format!("{id}:no-blob-ref"));
            continue;
        };
        let got = ro.get_blob(blob).map(|mut r| {
            let mut out = Vec::new();
            r.read_to_end(&mut out).map(|_| out)
        });
        match got {
            Ok(Ok(out)) if out == reader_bytes(id) => {}
            Ok(Ok(_)) => bad.push(format!("{id}:other-bytes")),
            Ok(Err(e)) => bad.push(format!("{id}:{}", format!("{e:?}").replace(' ', "_"))),
            Err(e) => bad.push(format!("{id}:{}", format!("{e:?}").replace(' ', "_"))),
        }
    }
    format!(
        "{} docs={} bad={}",
        ro.database().seq(),
        ids.len(),
        bad.join(",")
    )
}

/// A reader process: opens read-only (taking a reader lease) and reports `ready <seq>`, then per stdin line:
/// `read` reports `read <read_all>`; `refresh` refreshes with a new read-only open and reports
/// `refreshed <read_all>`, or `refresh-error <e>`.
fn reader(path: &Path) {
    let options = DatabaseOptions::default();
    let mut ro = ReadOnlyBlobDatabase::open(path, &options).unwrap();
    assert!(ro.lease().is_some(), "the reader holds a lease");
    report(&format!("ready {}", ro.database().seq()));
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        match line.as_str() {
            "read" => report(&format!("read {}", read_all(&ro))),
            "refresh" => {
                let reopened = ReadOnlyDatabase::open(path, &options.registry).unwrap();
                match ro.refresh(reopened) {
                    Ok(_) => report(&format!("refreshed {}", read_all(&ro))),
                    Err(e) => report(&format!("refresh-error {e:?}").replace('\n', " ")),
                }
            }
            other => panic!("reader: unknown command {other}"),
        }
    }
}

/// Holds the database read-write (both writer locks) until stdin closes; reports `opened`.
fn holder(path: &Path) {
    let db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    report("opened");
    wait_for_eof();
    drop(db);
}

/// Tries one read-write open and reports `locked` (the container's writer lock), or `opened` / `error <e>`.
fn try_writer(path: &Path) {
    match BlobDatabase::open(path, DatabaseOptions::default()) {
        Err(Error::Database(e))
            if matches!(*e, DatabaseError::Container(ContainerError::Locked)) =>
        {
            report("locked");
        }
        Ok(db) => {
            report("opened");
            drop(db);
        }
        Err(e) => report(&format!("error {e:?}").replace('\n', " ")),
    }
    wait_for_eof();
}

// ---------------------------------------------------------------------------------------------------------------
// Parent helpers
// ---------------------------------------------------------------------------------------------------------------

fn arg(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// The pack beside the container at `path`.
fn pack(path: &Path) -> PathBuf {
    persia::internal::pack_path(path)
}

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

/// Sends `SIGSTOP` or `SIGCONT` to `pid` with the `kill` utility (this crate has no `libc`).
fn signal(pid: u32, sig: &str) {
    let status = Command::new("kill")
        .args([sig, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {sig} {pid}: {status}");
}

/// The pack rewrite's temporary files beside the pack (`<name>.persia.blobs.tmp.*`, SPEC §9.3) and their sizes. A
/// file removed between the listing and its size is skipped.
fn temp_files(path: &Path) -> Vec<(PathBuf, u64)> {
    let dir = path.parent().unwrap();
    let mut prefix = pack(path).file_name().unwrap().to_owned();
    prefix.push(".tmp.");
    let prefix = prefix.to_str().unwrap().to_owned();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_str().unwrap().starts_with(&prefix)
            && let Ok(m) = entry.metadata()
        {
            out.push((entry.path(), m.len()));
        }
    }
    out
}

/// The pack's inode number.
fn pack_inode(path: &Path) -> u64 {
    std::fs::metadata(pack(path)).unwrap().ino()
}

/// `persia gc --json <path>`, spawned, with its stdout and stderr piped (it writes them only at its end).
fn spawn_gc(path: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_persia"))
        .args(["gc", arg(path), "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// The JSON report of a `persia gc` that exited 0.
fn gc_report(out: &Output, ctx: &str) -> Json {
    assert!(
        out.status.success(),
        "{ctx}: persia gc exited {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    assert_eq!(text.lines().count(), 1, "{ctx}: {text}");
    serde_json::from_str(&text).unwrap()
}

/// `persia gc --json <path>`, which must succeed: its report.
fn gc_ok(path: &Path, ctx: &str) -> Json {
    gc_report(&persia(&["gc", arg(path), "--json"]), ctx)
}

/// What `persia verify` reported about the blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Verified {
    /// Distinct blobs the references name.
    referenced: u64,
    /// Blobs the pack has bound.
    bound: u64,
    /// `BlobChunk` frames no bound blob names.
    orphan_chunks: u64,
    /// `BlobIndex` frames.
    indexes: u64,
}

/// The leading number of `text`.
fn lead(text: &str) -> u64 {
    text.split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap_or_else(|e| panic!("no number at the start of {text:?}: {e}"))
}

/// `persia verify [--deep] <path>`, which must pass: its blob counts.
fn verify(path: &Path, deep: bool, ctx: &str) -> Verified {
    let args: &[&str] = if deep { &["--deep"] } else { &[] };
    let rows = verify_ok(args, path, ctx);
    assert_eq!(row(&rows, "verify"), "ok", "{ctx}");
    // `<references> to <blobs> blobs`, `<n> blobs bound`, `<f> frames: <c> chunks (<o> orphans), <i> indexes`.
    let refs = row(&rows, "references");
    let referenced = lead(refs.split_once(" to ").unwrap().1);
    let bound = lead(row(&rows, "pack"));
    let scan = row(&rows, "scan");
    let orphan_chunks = lead(scan.split_once('(').unwrap().1);
    let indexes = lead(scan.rsplit_once(", ").unwrap().1);
    Verified {
        referenced,
        bound,
        orphan_chunks,
        indexes,
    }
}

/// A read-only open shows exactly the documents of `model`, each blob read whole and byte-identical.
fn check_reads(path: &Path, model: &Model, ctx: &str) {
    let ro = ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default())
        .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"));
    let snapshot = ro.database().snapshot();
    let shown = live_ids(&snapshot, COLLECTION);
    let expected: BTreeSet<String> = model.keys().cloned().collect();
    assert_eq!(shown, expected, "{ctx}: the documents shown");
    let c = snapshot.collection(COLLECTION).unwrap();
    for (id, &(size, cseed)) in model {
        let doc = c.get(id).unwrap().unwrap();
        let blob = doc
            .blob_ref()
            .unwrap_or_else(|| panic!("{ctx}: {id} has no blob"));
        assert_eq!(blob.size(), size, "{ctx}: {id}: size");
        let mut out = Vec::new();
        ro.get_blob(blob)
            .unwrap_or_else(|e| panic!("{ctx}: get_blob({id}): {e}"))
            .read_to_end(&mut out)
            .unwrap_or_else(|e| panic!("{ctx}: reading {id}'s blob: {e}"));
        assert!(
            out == content(size, cseed),
            "{ctx}: {id}: blob {} reads back other bytes ({} of {size})",
            blob.blob_id(),
            out.len()
        );
    }
}

/// Puts document `$id` with the blob `content($size, $cseed)` at `$durability`, and records it in `$model`.
macro_rules! put {
    ($db:expr, $model:expr, $id:expr, $size:expr, $cseed:expr, $durability:expr) => {{
        let (id, size, cseed): (&str, u64, u64) = ($id, $size, $cseed);
        let bytes = content(size, cseed);
        $db.put(
            COLLECTION,
            &Document::new(id),
            Blob::from_reader(&bytes[..]),
            $durability,
        )
        .unwrap();
        $model.insert(id.to_owned(), (size, cseed));
    }};
}

/// Deletes document `$id` at `$durability`, releases its pins, and drops it from `$model`.
macro_rules! delete {
    ($db:expr, $model:expr, $id:expr, $durability:expr) => {{
        let id: &str = $id;
        $db.log_mut().delete(COLLECTION, id, $durability).unwrap();
        $db.release_durable();
        $model.remove(id);
    }};
}

/// A small document's blob: a seeded size of at most 200 KiB, or a pooled duplicate.
fn small_blob(rng: &mut Rng) -> (u64, u64) {
    if rng.chance(15) {
        POOL[usize::try_from(rng.range(0, POOL.len() as u64)).unwrap()]
    } else {
        (pick_size(rng, 0), rng.next_u64())
    }
}

/// A random document id of `model` whose id starts with `prefix`, if any.
fn pick_id(rng: &mut Rng, model: &Model, prefix: &str) -> Option<String> {
    let ids: Vec<&String> = model.keys().filter(|id| id.starts_with(prefix)).collect();
    if ids.is_empty() {
        return None;
    }
    let i = usize::try_from(rng.range(0, ids.len() as u64)).unwrap();
    Some(ids[i].clone())
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Crash safety
// ---------------------------------------------------------------------------------------------------------------

/// When the parent kills a `persia gc` run (observed on the file system, never timed).
#[derive(Debug, Clone, Copy)]
enum KillAt {
    /// Right after the spawn.
    Spawned,
    /// After this many directory polls.
    Polls(u64),
    /// Once a rewrite temporary file is listed.
    TempSeen,
    /// Once a rewrite temporary file holds at least this many bytes.
    TempBytes(u64),
    /// Once the pack's inode changed (the rename happened).
    Renamed,
    /// Not killed.
    Never,
}

/// What became of one `persia gc` run.
#[derive(Debug)]
enum Ended {
    /// Killed: whether a temporary file was listed, and whether the pack had been renamed, at the last poll.
    Killed { temp: bool, renamed: bool },
    /// It exited first: its output.
    Exited(Output),
}

/// Runs `persia gc` on `path` and kills it at `at`, polling the directory in a loop (no sleep) until then.
fn run_gc_until(path: &Path, at: KillAt) -> Ended {
    let inode = pack_inode(path);
    let mut guard = Reaped(Some(spawn_gc(path)));
    let mut polls = 0_u64;
    loop {
        let child = guard.0.as_mut().unwrap();
        if child.try_wait().unwrap().is_some() {
            let child = guard.0.take().unwrap();
            return Ended::Exited(child.wait_with_output().unwrap());
        }
        let temps = temp_files(path);
        let renamed = std::fs::metadata(pack(path)).is_ok_and(|m| m.ino() != inode);
        let hit = match at {
            KillAt::Spawned => true,
            KillAt::Polls(n) => polls >= n,
            KillAt::TempSeen => !temps.is_empty(),
            KillAt::TempBytes(b) => temps.iter().any(|(_, len)| *len >= b),
            KillAt::Renamed => renamed,
            KillAt::Never => false,
        };
        if hit {
            let mut child = guard.0.take().unwrap();
            child.kill().unwrap(); // SIGKILL; also succeeds on a child that exited but is not reaped yet.
            let out = child.wait_with_output().unwrap();
            if out.status.signal().is_none() {
                return Ended::Exited(out); // It exited by itself between the poll and the kill.
            }
            return Ended::Killed {
                temp: !temps.is_empty(),
                renamed,
            };
        }
        polls += 1;
    }
}

#[derive(Debug, Default)]
struct CrashStats {
    runs: u64,
    killed: u64,
    killed_before_rewrite: u64,
    killed_mid_rewrite: u64,
    killed_after_rename: u64,
    exited: u64,
    stale_temps: u64,
    truncated_tails: u64,
    reruns_done: u64,
    reruns_below_ratio: u64,
    tails: u64,
}

/// One writer round of the crash scenario on `path`: replaces the big documents, stores and deletes a garbage one,
/// churns small ones, commits, and maybe leaves an uncommitted `Fsync` tail. Keeps the dead bytes at least the kept
/// ones after the first round (the big documents' old versions plus the garbage one outweigh the small live ones), so
/// a run reaches the default rewrite ratio from then on.
fn crash_round(path: &Path, round: u64, rng: &mut Rng, model: &mut Model, stats: &mut CrashStats) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    for k in 0..2 {
        let size = [CHUNK - 1, CHUNK, CHUNK + 1][usize::try_from(rng.range(0, 3)).unwrap()];
        put!(
            db,
            model,
            &format!("big-{k}"),
            size,
            rng.next_u64(),
            Durability::Fsync
        );
    }
    let garbage = format!("garbage-{round}");
    put!(
        db,
        model,
        &garbage,
        2 * CHUNK + rng.range(1, 1 << 16),
        rng.next_u64(),
        Durability::Fsync
    );
    delete!(db, model, &garbage, Durability::Fsync);
    for n in 0..rng.range(4, 12) {
        let small = model.keys().filter(|id| id.starts_with("s-")).count();
        let roll = rng.range(0, 100);
        if roll < 50 && small < 12 {
            let (size, cseed) = small_blob(rng);
            put!(
                db,
                model,
                &format!("s-{round}-{n}"),
                size,
                cseed,
                Durability::Fsync
            );
        } else if roll < 75
            && let Some(id) = pick_id(rng, model, "s-")
        {
            let (size, cseed) = small_blob(rng);
            put!(db, model, &id, size, cseed, Durability::Fsync);
        } else if let Some(id) = pick_id(rng, model, "s-") {
            delete!(db, model, &id, Durability::Fsync);
        }
    }
    db.commit().unwrap();
    if rng.chance(40) {
        // An uncommitted tail, logged `Fsync`: `persia gc`'s open replays it, and its mark keeps what it names.
        stats.tails += 1;
        for n in 0..rng.range(1, 4) {
            match (rng.range(0, 3), pick_id(rng, model, "s-")) {
                (0, Some(id)) => delete!(db, model, &id, Durability::Fsync),
                (1, Some(id)) => {
                    let (size, cseed) = small_blob(rng);
                    put!(db, model, &id, size, cseed, Durability::Fsync);
                }
                _ => {
                    let (size, cseed) = small_blob(rng);
                    put!(
                        db,
                        model,
                        &format!("s-{round}-t{n}"),
                        size,
                        cseed,
                        Durability::Fsync
                    );
                }
            }
        }
    }
    drop(db);
}

/// The checks of a `persia gc` run's end (module docs, invariant 1) that hold whatever its outcome.
fn check_after_run(path: &Path, model: &Model, deep: bool, ctx: &str) -> Verified {
    let v = verify(path, deep, ctx);
    assert!(v.bound >= v.referenced, "{ctx}: {v:?}");
    check_reads(path, model, ctx);
    v
}

/// One round's `persia gc` run, killed at `at`, and the checks after it, then the rerun and its checks.
fn crash_run(path: &Path, at: KillAt, model: &Model, stats: &mut CrashStats, ctx: &str) {
    let pack_before = std::fs::read(pack(path)).unwrap();
    let inode_before = pack_inode(path);
    stats.runs += 1;
    let ended = match run_gc_until(path, at) {
        Ended::Exited(out) => {
            stats.exited += 1;
            let done = gc_report(&out, ctx);
            assert_eq!(done["held_back"], false, "{ctx}: {done}");
            assert_eq!(done["missing"], serde_json::json!([]), "{ctx}: {done}");
            format!("exited, rewrite {}", done["rewrite"]["outcome"])
        }
        Ended::Killed { temp, renamed } => {
            stats.killed += 1;
            let at = match (temp, renamed) {
                (_, true) => {
                    stats.killed_after_rename += 1;
                    "after the rename"
                }
                (true, false) => {
                    stats.killed_mid_rewrite += 1;
                    "mid-rewrite"
                }
                (false, false) => {
                    stats.killed_before_rewrite += 1;
                    "before any rewrite file"
                }
            };
            stats.stale_temps += u64::try_from(temp_files(path).len()).unwrap();
            format!("killed {at}")
        }
    };
    let ctx_kill = format!("{ctx}: after the run");
    let v = check_after_run(path, model, true, &ctx_kill);
    if pack_inode(path) == inode_before {
        // The open may truncate the chunks a deduplicated put staged after `log_end` (SPEC §9.3), nothing else.
        let now = std::fs::read(pack(path)).unwrap();
        let first_diff = now.iter().zip(&pack_before).position(|(a, b)| a != b);
        assert!(
            pack_before.starts_with(&now),
            "{ctx_kill}: the pack was not replaced, yet its bytes changed (the rewrite is never in place): \
             {} -> {} bytes, first difference at {first_diff:?}",
            pack_before.len(),
            now.len()
        );
        stats.truncated_tails += u64::from(now.len() < pack_before.len());
    } else {
        assert_eq!(v.bound, v.referenced, "{ctx_kill}: replaced pack {v:?}");
        assert_eq!(v.orphan_chunks, 0, "{ctx_kill}: replaced pack {v:?}");
        assert_eq!(v.indexes, v.bound, "{ctx_kill}: replaced pack {v:?}");
    }

    // The rerun completes, sweeps any temporary file the kill left, and leaves exactly the referenced blobs when it
    // rewrites.
    let ctx_rerun = format!("{ctx}: rerun");
    let again = gc_ok(path, &ctx_rerun);
    assert_eq!(again["held_back"], false, "{ctx_rerun}: {again}");
    assert_eq!(
        again["missing"],
        serde_json::json!([]),
        "{ctx_rerun}: {again}"
    );
    assert_eq!(
        again["marked"]["blobs"], v.referenced,
        "{ctx_rerun}: {again}"
    );
    assert!(
        temp_files(path).is_empty(),
        "{ctx_rerun}: temporary files left: {:?}",
        temp_files(path)
    );
    let after = check_after_run(path, model, false, &ctx_rerun);
    eprintln!(
        "{ctx}: {ended}; rerun rewrite {}",
        again["rewrite"]["outcome"]
    );
    match again["rewrite"]["outcome"].as_str().unwrap() {
        "done" => {
            stats.reruns_done += 1;
            assert_eq!(after.bound, after.referenced, "{ctx_rerun}: {after:?}");
            assert_eq!(after.orphan_chunks, 0, "{ctx_rerun}: {after:?}");
            assert_eq!(
                again["deleted"]["blobs"],
                v.bound - v.referenced,
                "{ctx_rerun}: {again}"
            );
        }
        "below_ratio" => {
            stats.reruns_below_ratio += 1;
            assert_eq!(again["deleted"]["blobs"], 0, "{ctx_rerun}: {again}");
            assert_eq!(after.bound, v.bound, "{ctx_rerun}: {after:?}");
        }
        other => panic!("{ctx_rerun}: rewrite {other}: {again}"),
    }
}

#[test]
fn kill_9_of_persia_gc_keeps_every_referenced_blob_and_a_rerun_completes() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x5eed_1045_9c01);
    let kills = env_count("PERSIA_TEST_KILLS", KILLS);
    let started = Instant::now();
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    let mut rng = Rng(seed);
    let mut model = Model::new();
    let mut stats = CrashStats::default();
    for round in 0..kills {
        crash_round(&path, round, &mut rng, &mut model, &mut stats);
        // Every kind of kill point in turn (the first round's run is below the ratio: no old big versions yet), with
        // seeded parameters.
        let at = match (round + 5) % 6 {
            0 => KillAt::Spawned,
            1 => KillAt::Polls(rng.range(1, 400)),
            2 => KillAt::TempSeen,
            3 => KillAt::TempBytes(rng.range(12_288, 3 * CHUNK)),
            4 => KillAt::Renamed,
            _ => KillAt::Never,
        };
        let ctx = format!("seed {seed} round {round} ({at:?})");
        crash_run(&path, at, &model, &mut stats, &ctx);
    }
    eprintln!(
        "{stats:?}, {} documents, pack {} MiB, total {:.1?}",
        model.len(),
        std::fs::metadata(pack(&path)).unwrap().len() >> 20,
        started.elapsed()
    );
    assert!(
        stats.killed_mid_rewrite > 0,
        "no kill landed inside a rewrite: {stats:?}"
    );
    assert!(
        stats.killed_before_rewrite > 0,
        "no kill landed before a rewrite file: {stats:?}"
    );
}

/// The options of an application that registers `tags@1`.
fn tags_options() -> DatabaseOptions {
    let mut registry = AnalyzerRegistry::builtin();
    registry
        .register("tags", 1, Analyzer::builder(WhitespaceTokenizer).build())
        .unwrap();
    DatabaseOptions {
        registry,
        ..DatabaseOptions::default()
    }
}

/// A database at `<dir>/app.persia` whose collection `COLLECTION` has a `tags` field naming `tags@1`: `seg` with its
/// blob committed in a segment, three garbage blobs (their documents deleted and committed: three quarters of the
/// pack's chunk bytes, so a run rewrites), and `wal` with its blob logged `Fsync` and never committed, so only the WAL
/// references it. The live documents and their `(size, content seed)`.
fn registered_analyzer_database(dir: &Path) -> (PathBuf, Model) {
    let path = dir.join("app.persia");
    let options = tags_options();
    let schema = CollectionSchema::from_json_in(
        &format!(r#"{{"name": "{COLLECTION}", "fields": {{"tags": {{"type": "text", "analyzer": "tags"}}}}}}"#),
        &options.registry,
    )
    .unwrap();
    let tags = schema.field("tags").unwrap().id();
    let tagged = |id: &str| Document::new(id).with(tags, Value::String("a b".into()));
    let mut db = BlobDatabase::create(&path, options).unwrap();
    db.log_mut().create_collection(schema.clone()).unwrap();
    let mut model = Model::new();
    let put = |db: &mut BlobDatabase<_>, id: &str, size: u64, cseed: u64| {
        let bytes = content(size, cseed);
        db.put(
            COLLECTION,
            &tagged(id),
            Blob::from_reader(&bytes[..]),
            Durability::Fsync,
        )
        .unwrap();
        (id.to_owned(), (size, cseed))
    };
    let (id, blob) = put(&mut db, "seg", CHUNK + 1000, 1);
    model.insert(id, blob);
    for (n, id) in ["g1", "g2", "g3"].into_iter().enumerate() {
        put(&mut db, id, CHUNK + 1000, 10 + n as u64);
    }
    db.commit().unwrap();
    for id in ["g1", "g2", "g3"] {
        db.log_mut()
            .delete(COLLECTION, id, Durability::Fsync)
            .unwrap();
    }
    db.commit().unwrap();
    let (id, blob) = put(&mut db, "wal", CHUNK / 2, 2);
    model.insert(id, blob);
    drop(db);
    (path, model)
}

/// `persia stats --json <path>`, which must succeed.
fn stats_json(path: &Path) -> Json {
    let out = persia(&["stats", arg(path), "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The application's read-only open (with `tags@1`) shows exactly `model`'s documents, `wal` still a WAL record, each
/// blob byte-identical.
fn check_app_reads(path: &Path, model: &Model, ctx: &str) {
    let ro = ReadOnlyBlobDatabase::open(path, &tags_options())
        .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"));
    let snapshot = ro.database().snapshot();
    let expected: BTreeSet<String> = model.keys().cloned().collect();
    assert_eq!(live_ids(&snapshot, COLLECTION), expected, "{ctx}");
    let c = snapshot.collection(COLLECTION).unwrap();
    let stats = stats_json(path);
    assert_eq!(
        stats["collections"][0]["wal_documents"], 1,
        "{ctx}: `wal` is no longer a WAL record: {stats}"
    );
    for (id, &(size, cseed)) in model {
        let doc = c.get(id).unwrap().unwrap();
        let blob = doc.blob_ref().unwrap();
        let mut out = Vec::new();
        ro.get_blob(blob)
            .unwrap_or_else(|e| panic!("{ctx}: get_blob({id}): {e}"))
            .read_to_end(&mut out)
            .unwrap();
        assert!(out == content(size, cseed), "{ctx}: {id}: other bytes");
    }
}

/// ROADMAP 10.4.7, ADR-0069 item 5: `persia gc` on a database whose schema names a registered analyzer (the mark-only
/// open, with stand-ins), killed (SIGKILL) at each kind of point the parent observes on the file system, with no timer.
/// After each kill `verify --deep` passes, a pack whose inode did not change is a prefix of the pack before the run
/// (the rewrite is never in place), the application reads every document and blob, the WAL-only one included, and that
/// record is still a WAL record (nothing was flushed with a stand-in); a rerun completes and leaves exactly the two
/// referenced blobs.
#[test]
fn kill_9_of_persia_gc_on_a_registered_analyzer_database_loses_no_blob() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x5eed_1047_0001);
    let mut rng = Rng(seed);
    let points = [
        KillAt::Spawned,
        KillAt::Polls(rng.range(1, 400)),
        KillAt::TempSeen,
        KillAt::TempBytes(rng.range(12_288, CHUNK)),
        KillAt::Renamed,
        KillAt::Never,
    ];
    let mut killed_mid_rewrite = 0;
    for at in points {
        let ctx = format!("seed {seed} ({at:?})");
        let dir = KeptOnFailure::new();
        let (path, model) = registered_analyzer_database(dir.path());
        let pack_before = std::fs::read(pack(&path)).unwrap();
        let inode = pack_inode(&path);
        let ended = match run_gc_until(&path, at) {
            Ended::Exited(out) => {
                let done = gc_report(&out, &ctx);
                assert_eq!(done["marked"]["blobs"], 2, "{ctx}: {done}");
                format!("exited, rewrite {}", done["rewrite"]["outcome"])
            }
            Ended::Killed { temp, renamed } => {
                killed_mid_rewrite += u64::from(temp && !renamed);
                format!("killed (temp {temp}, renamed {renamed})")
            }
        };
        eprintln!("{ctx}: {ended}");
        let v = verify(&path, true, &ctx);
        assert_eq!(v.referenced, 2, "{ctx}: {v:?}");
        if pack_inode(&path) == inode {
            let now = std::fs::read(pack(&path)).unwrap();
            assert!(
                pack_before.starts_with(&now),
                "{ctx}: the pack was not replaced, yet its bytes changed"
            );
        } else {
            assert_eq!(v.bound, 2, "{ctx}: replaced pack {v:?}");
        }
        check_app_reads(&path, &model, &ctx);

        let again = gc_ok(&path, &ctx);
        assert_eq!(again["marked"]["blobs"], 2, "{ctx}: rerun {again}");
        assert_eq!(again["missing"], serde_json::json!([]), "{ctx}: {again}");
        assert!(temp_files(&path).is_empty(), "{ctx}: temporary files left");
        let after = verify(&path, true, &ctx);
        assert_eq!((after.referenced, after.bound), (2, 2), "{ctx}: {after:?}");
        assert_eq!(after.orphan_chunks, 0, "{ctx}: {after:?}");
        check_app_reads(&path, &model, &ctx);
    }
    assert!(
        killed_mid_rewrite > 0,
        "seed {seed}: no kill landed inside a rewrite"
    );
}
// ---------------------------------------------------------------------------------------------------------------
// 2. Reader processes
// ---------------------------------------------------------------------------------------------------------------

/// Puts `r1`..`r4`, each with its blob (`reader_bytes`), and commits; then, when `orphan`, deletes `r2`..`r4` and
/// commits again.
fn reader_scenario(path: &Path, orphan: bool) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    for n in 1..=4 {
        let id = format!("r{n}");
        let bytes = reader_bytes(&id);
        db.put(
            COLLECTION,
            &Document::new(&id),
            Blob::from_reader(&bytes[..]),
            Durability::Fsync,
        )
        .unwrap();
    }
    db.commit().unwrap();
    if orphan {
        for id in ["r2", "r3", "r4"] {
            db.log_mut()
                .delete(COLLECTION, id, Durability::Fsync)
                .unwrap();
        }
        db.release_durable();
        db.commit().unwrap();
    }
}

/// Deletes `ids` (`Fsync`) in a writer opened for it, and commits: puts every reader behind.
fn delete_and_commit(path: &Path, ids: &[&str]) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    for id in ids {
        db.log_mut()
            .delete(COLLECTION, id, Durability::Fsync)
            .unwrap();
    }
    db.release_durable();
    db.commit().unwrap();
}

/// A reader's report `<word> <seq> docs=<n> bad=<what>`: its seq and document count, with nothing bad.
fn read_ok(line: &str, word: &str, ctx: &str) -> (u64, u64) {
    let rest = line
        .strip_prefix(word)
        .and_then(|r| r.strip_prefix(' '))
        .unwrap_or_else(|| panic!("{ctx}: {line}"));
    let mut words = rest.split(' ');
    let seq = words.next().unwrap().parse().unwrap();
    let docs = words
        .next()
        .and_then(|w| w.strip_prefix("docs="))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(words.next(), Some("bad="), "{ctx}: {line}");
    (seq, docs)
}

/// The lease files in the leases directory of the database at `path`.
fn lease_files(path: &Path) -> BTreeSet<PathBuf> {
    match std::fs::read_dir(path.with_extension("persia.blobs.leases")) {
        Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// Puts document `id` with its blob (`reader_bytes`, `Fsync`) in a writer opened for it and commits: returns the seq.
fn put_and_commit(path: &Path, id: &str) -> u64 {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    let bytes = reader_bytes(id);
    db.put(
        COLLECTION,
        &Document::new(id),
        Blob::from_reader(&bytes[..]),
        Durability::Fsync,
    )
    .unwrap();
    db.commit().unwrap()
}

/// The reader counts of a `persia gc` report: `(live, current, behind, opening + ahead + unreadable, stale_removed,
/// oldest_seq)`.
fn readers(report: &Json) -> (u64, u64, u64, u64, u64, Option<u64>) {
    let r = &report["readers"];
    let n = |k: &str| {
        r[k].as_u64()
            .unwrap_or_else(|| panic!("readers.{k}: {report}"))
    };
    (
        n("live"),
        n("current"),
        n("behind"),
        n("opening") + n("ahead") + n("unreadable"),
        n("stale_removed"),
        r["oldest_seq"].as_u64(),
    )
}

/// A reader process opened before the last commit is behind: `persia gc` marks, but deletes and rewrites nothing, the
/// pack is byte-identical, the report counts it behind, and the reader still reads the blobs of the documents the
/// writer deleted. Once it refreshes (current), the next run sweeps and rewrites; the reader reads its blobs on after
/// the rewrite, and again after a refresh, which reopens the replaced pack.
#[test]
fn a_reader_process_behind_the_committed_seq_holds_persia_gc_back_until_it_refreshes() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    if !locks_judged(dir.path()) {
        return;
    }
    let path = create(dir.path());
    reader_scenario(&path, false);
    let mut reader = ChildIo::spawn("reader", &path, 0, "r", 0);
    let ready = reader.expect("reader ready");
    let opened: u64 = ready.strip_prefix("ready ").unwrap().parse().unwrap();
    delete_and_commit(&path, &["r2", "r3", "r4"]);
    let committed = ReadOnlyDatabase::open(&path, &DatabaseOptions::default().registry)
        .unwrap()
        .seq();
    assert!(committed > opened);
    let before = std::fs::read(pack(&path)).unwrap();

    let held = gc_ok(&path, "behind");
    assert_eq!(held["held_back"], true, "{held}");
    assert_eq!(readers(&held), (1, 0, 1, 0, 0, Some(opened)), "{held}");
    assert_eq!(held["marked"]["blobs"], 1, "the mark ran: {held}");
    assert_eq!(
        held["deleted"],
        serde_json::json!({"blobs": 0, "uploads": 0, "chunks": 0}),
        "{held}"
    );
    assert_eq!(held["rewrite"]["outcome"], "not_run", "{held}");
    assert_eq!(held["pack"]["bytes_before"], before.len(), "{held}");
    assert_eq!(held["pack"]["bytes_after"], before.len(), "{held}");
    assert!(
        std::fs::read(pack(&path)).unwrap() == before,
        "the pack is untouched"
    );
    let text = persia(&["gc", arg(&path)]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains("sweep             held back by reader leases: nothing deleted\n"),
        "{text}"
    );
    assert!(
        text.contains("readers           1 live: 0 current, 0 opening, 1 behind"),
        "{text}"
    );
    // The reader still shows, and reads, the documents the writer deleted.
    reader.send("read");
    assert_eq!(
        read_ok(&reader.expect("read"), "read", "behind"),
        (opened, 4)
    );
    let v = verify(&path, false, "behind");
    assert_eq!((v.referenced, v.bound), (1, 4), "{v:?}");

    reader.send("refresh");
    assert_eq!(
        read_ok(&reader.expect("refresh"), "refreshed", "refresh"),
        (committed, 1)
    );
    let swept = gc_ok(&path, "current");
    assert_eq!(swept["held_back"], false, "{swept}");
    assert_eq!(readers(&swept), (1, 1, 0, 0, 0, Some(committed)), "{swept}");
    assert_eq!(swept["deleted"]["blobs"], 3, "{swept}");
    assert_eq!(swept["rewrite"]["outcome"], "done", "{swept}");
    assert!(
        swept["pack"]["bytes_after"].as_u64().unwrap() < before.len() as u64 / 2,
        "{swept}"
    );
    reader.send("read");
    assert_eq!(
        read_ok(&reader.expect("read after"), "read", "after the rewrite"),
        (committed, 1)
    );
    reader.send("refresh");
    assert_eq!(
        read_ok(
            &reader.expect("refresh after"),
            "refreshed",
            "refresh after the rewrite"
        ),
        (committed, 1)
    );
    let v = verify(&path, true, "after");
    assert_eq!((v.referenced, v.bound, v.orphan_chunks), (1, 1, 0), "{v:?}");
    assert_eq!(lease_files(&path).len(), 1, "the reader's lease");
    reader.finish();
    assert!(
        lease_files(&path).is_empty(),
        "a dropped reader removes its lease"
    );
}

/// A reader process at exactly the committed seq lets `persia gc` sweep and rewrite; it reads its blobs on after the
/// rewrite (it keeps the old pack file) and after a refresh (which reopens the new one), and its lease is not taken
/// for stale. A commit puts it behind (held back) until it refreshes; then a run with nothing dead rewrites nothing.
/// Once the reader is killed, its lease is stale and the next run removes it.
#[test]
fn a_reader_process_at_the_committed_seq_lets_persia_gc_sweep_and_reads_on_after_the_rewrite() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    if !locks_judged(dir.path()) {
        return;
    }
    let path = create(dir.path());
    reader_scenario(&path, true);
    let mut reader = ChildIo::spawn("reader", &path, 0, "r", 0);
    let ready = reader.expect("reader ready");
    let opened: u64 = ready.strip_prefix("ready ").unwrap().parse().unwrap();
    let before = std::fs::metadata(pack(&path)).unwrap().len();

    let swept = gc_ok(&path, "current");
    assert_eq!(swept["held_back"], false, "{swept}");
    assert_eq!(readers(&swept), (1, 1, 0, 0, 0, Some(opened)), "{swept}");
    assert_eq!(swept["deleted"]["blobs"], 3, "{swept}");
    assert_eq!(swept["rewrite"]["outcome"], "done", "{swept}");
    assert!(
        swept["pack"]["bytes_after"].as_u64().unwrap() < before / 2,
        "{swept}"
    );
    reader.send("read");
    assert_eq!(
        read_ok(&reader.expect("read"), "read", "after the rewrite"),
        (opened, 1)
    );
    reader.send("refresh");
    assert_eq!(
        read_ok(&reader.expect("refresh"), "refreshed", "refresh"),
        (opened, 1)
    );
    assert_eq!(lease_files(&path).len(), 1, "the live lease stays");

    // A commit puts the reader behind; once it refreshed, nothing is dead.
    let committed = put_and_commit(&path, "r5");
    let held = gc_ok(&path, "behind");
    assert_eq!(held["held_back"], true, "{held}");
    assert_eq!(readers(&held), (1, 0, 1, 0, 0, Some(opened)), "{held}");
    reader.send("refresh");
    assert_eq!(
        read_ok(&reader.expect("refresh 2"), "refreshed", "refresh 2"),
        (committed, 2)
    );
    let idle = gc_ok(&path, "nothing dead");
    assert_eq!(idle["held_back"], false, "{idle}");
    assert_eq!(readers(&idle), (1, 1, 0, 0, 0, Some(committed)), "{idle}");
    assert_eq!(idle["deleted"]["blobs"], 0, "{idle}");
    assert_eq!(idle["rewrite"]["outcome"], "below_ratio", "{idle}");
    reader.send("read");
    assert_eq!(
        read_ok(&reader.expect("read 2"), "read", "read 2"),
        (committed, 2)
    );

    // A killed reader leaves a stale lease, which the next run removes.
    reader.kill();
    assert_eq!(
        lease_files(&path).len(),
        1,
        "the killed reader's lease file"
    );
    let stale = gc_ok(&path, "stale");
    assert_eq!(stale["held_back"], false, "{stale}");
    assert_eq!(readers(&stale), (0, 0, 0, 0, 1, None), "{stale}");
    assert!(lease_files(&path).is_empty(), "the stale lease is removed");
    let v = verify(&path, true, "end");
    assert_eq!((v.referenced, v.bound, v.orphan_chunks), (2, 2, 0), "{v:?}");
}

// ---------------------------------------------------------------------------------------------------------------
// 3. Writer processes
// ---------------------------------------------------------------------------------------------------------------

/// Every regular file under `dir` (the leases directory's included) and its bytes.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                todo.push(entry.path());
            } else if kind.is_file() {
                out.insert(entry.path(), std::fs::read(entry.path()).unwrap());
            }
        }
    }
    out
}

/// A `persia gc` that must fail because a writer holds the database: status 1, nothing on stdout, and the message.
fn gc_locked_out(path: &Path, ctx: &str) {
    for json in [false, true] {
        let mut args = vec!["gc", arg(path)];
        if json {
            args.push("--json");
        }
        let out = persia(&args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{ctx}: {err}");
        assert_eq!(out.stdout, b"", "{ctx}");
        assert!(
            err.contains("is open by a writer; collect it once that is closed"),
            "{ctx}: {err}"
        );
    }
}

/// While a writer process holds the database, `persia gc` (text and `--json`) exits 1 and changes no byte of any
/// file; once the writer exits, it collects.
#[test]
fn persia_gc_exits_1_and_changes_nothing_while_a_writer_process_holds_the_database() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    reader_scenario(&path, true);
    let holder = ChildIo::spawn("holder", &path, 0, "w", 0);
    assert_eq!(holder.expect("holder"), "opened");
    let before = files(dir.path());
    gc_locked_out(&path, "writer process");
    assert!(files(dir.path()) == before, "no file changed");
    holder.finish();
    let done = gc_ok(&path, "after the writer");
    assert_eq!(done["deleted"]["blobs"], 3, "{done}");
    assert_eq!(done["rewrite"]["outcome"], "done", "{done}");
}

/// Kills a `persia gc` child left running (or stopped) by a failed assertion, so no process outlives the test.
struct Reaped(Option<Child>);

impl Drop for Reaped {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill(); // Cleanup on a failure path: the child may have exited already.
            let _ = child.wait();
        }
    }
}

/// Whether the process `pid` is stopped, as far as the system says: on Linux its `/proc/<pid>/stat` state is `T` (or
/// `t`); elsewhere `None` (unknown).
fn is_stopped(pid: u32) -> Option<bool> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ...`: the state follows the last `)` (the command name may hold one).
    let state = stat.rsplit_once(')')?.1.split_whitespace().next()?;
    Some(matches!(state, "T" | "t"))
}

/// How many times [`a_writer_process_and_a_second_persia_gc_fail_while_a_stopped_persia_gc_holds_the_database`]
/// rebuilds and retries when its `SIGSTOP` lands too late (after the rename, or after the exit).
const STOP_ATTEMPTS: usize = 5;

/// A `persia gc` stopped (`SIGSTOP`) once its rewrite's temporary file appears holds both writer locks: a writer
/// process's open fails on the container's lock, a second `persia gc` exits 1, and no file changes meanwhile. Resumed
/// (`SIGCONT`), the first run completes: the orphans deleted, the referenced blob intact.
///
/// The signal is sent by the `kill` utility (no `libc` here), so it can land after the rename, or after the exit. Such
/// an attempt is a miss, detected (the child exited, or the temporary file is gone once it is stopped), resumed, reaped
/// and retried on a new database, up to [`STOP_ATTEMPTS`] times; a caught attempt is checked in full.
#[test]
fn a_writer_process_and_a_second_persia_gc_fail_while_a_stopped_persia_gc_holds_the_database() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    for attempt in 1..=STOP_ATTEMPTS {
        if stopped_mid_rewrite(attempt) {
            return;
        }
        eprintln!(
            "attempt {attempt}: SIGSTOP landed after the rewrite; retrying on a new database"
        );
    }
    panic!("no attempt in {STOP_ATTEMPTS} stopped persia gc during its rewrite");
}

/// One attempt of the stopped-`persia gc` test: `false` when the stop missed the rewrite (nothing checked), `true` once
/// a caught attempt passed every check.
fn stopped_mid_rewrite(attempt: usize) -> bool {
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    // One kept blob of about six chunks and three orphans of about three each (60% of the frames dead, so the pack is
    // rewritten): a rewrite that copies and syncs 24 MiB, a wide window for the stop.
    let mut model = Model::new();
    {
        let mut db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
        put!(db, model, "f1", 6 * CHUNK + 1, 1, Durability::Fsync);
        for n in 2..=4_u64 {
            put!(
                db,
                model,
                &format!("f{n}"),
                3 * CHUNK + n,
                n,
                Durability::Fsync
            );
        }
        db.commit().unwrap();
        for n in 2..=4 {
            delete!(db, model, &format!("f{n}"), Durability::Fsync);
        }
        db.commit().unwrap();
    }
    let mut gc = Reaped(Some(spawn_gc(&path)));
    // Poll (no sleep) until the rewrite's temporary file is listed. A poll takes microseconds.
    loop {
        let child = gc.0.as_mut().unwrap();
        if let Some(status) = child.try_wait().unwrap() {
            // It finished before a poll saw its temporary file: a miss, but a run that succeeded.
            gc.0 = None;
            assert!(
                status.success(),
                "attempt {attempt}: the missed persia gc failed: {status}"
            );
            return false;
        }
        if !temp_files(&path).is_empty() {
            break;
        }
    }
    let pid = gc.0.as_ref().unwrap().id();
    signal(pid, "-STOP");
    // Wait (no sleep) until the system shows it stopped, where it can tell; it may also have exited meanwhile.
    loop {
        if let Some(status) = gc.0.as_mut().unwrap().try_wait().unwrap() {
            // It exited before the stop took: a miss, but a run that succeeded.
            gc.0 = None;
            assert!(
                status.success(),
                "attempt {attempt}: the missed persia gc failed: {status}"
            );
            return false;
        }
        if is_stopped(pid) != Some(false) {
            break;
        }
        std::thread::yield_now();
    }
    if temp_files(&path).len() != 1 {
        // Stopped after the rename: resume it, let it finish, and try again.
        signal(pid, "-CONT");
        let status = gc.0.take().unwrap().wait().unwrap();
        assert!(
            status.success(),
            "attempt {attempt}: the missed persia gc failed: {status}"
        );
        return false;
    }
    let before = files(dir.path());
    assert_eq!(temp_files(&path).len(), 1, "the rewrite's temporary file");

    let try_writer = ChildIo::spawn("try-writer", &path, 0, "w", 0);
    assert_eq!(try_writer.expect("try-writer"), "locked");
    try_writer.finish();
    gc_locked_out(&path, "beside a stopped persia gc");
    assert!(
        files(dir.path()) == before,
        "no file changed while persia gc was stopped"
    );

    signal(pid, "-CONT");
    let out = gc.0.take().unwrap().wait_with_output().unwrap();
    let done = gc_report(&out, "resumed");
    assert_eq!(done["held_back"], false, "{done}");
    assert_eq!(done["deleted"]["blobs"], 3, "{done}");
    assert_eq!(done["rewrite"]["outcome"], "done", "{done}");
    assert!(
        temp_files(&path).is_empty(),
        "the temporary file was renamed"
    );
    let v = verify(&path, true, "resumed");
    assert_eq!((v.referenced, v.bound, v.orphan_chunks), (1, 1, 0), "{v:?}");
    check_reads(&path, &model, "resumed");
    true
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Soak
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct SoakStats {
    puts: u64,
    replaces: u64,
    deletes: u64,
    commits: u64,
    tails: u64,
    rewrites: u64,
    below_ratio: u64,
    deleted: u64,
}

/// One soak round's writes on `path`: seeded puts, replaces and deletes (`Fsync` or `Async`) and commits, ending
/// durable: with a commit, or with an `Fsync` put that syncs the records before it (an uncommitted tail).
fn soak_round(path: &Path, round: u64, rng: &mut Rng, model: &mut Model, stats: &mut SoakStats) {
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    let mut synced = true;
    for op in 0..SOAK_OPS {
        let roll = rng.range(0, 100);
        let (durability, fsync) = if rng.chance(70) {
            (Durability::Fsync, true)
        } else {
            (Durability::Async, false)
        };
        let blob = |rng: &mut Rng| {
            if rng.chance(10) {
                POOL[usize::try_from(rng.range(0, POOL.len() as u64)).unwrap()]
            } else {
                (pick_size(rng, 2), rng.next_u64())
            }
        };
        if roll < 40 || model.is_empty() {
            let (size, cseed) = blob(rng);
            put!(
                db,
                model,
                &format!("d-{round}-{op}"),
                size,
                cseed,
                durability
            );
            stats.puts += 1;
            synced = fsync;
        } else if roll < 62 {
            let id = pick_id(rng, model, "d-").unwrap();
            let (size, cseed) = blob(rng);
            put!(db, model, &id, size, cseed, durability);
            stats.replaces += 1;
            synced = fsync;
        } else if roll < 85 {
            let id = pick_id(rng, model, "d-").unwrap();
            delete!(db, model, &id, durability);
            stats.deletes += 1;
            synced = fsync;
        } else {
            db.commit().unwrap();
            stats.commits += 1;
            synced = true;
        }
    }
    if rng.chance(70) {
        db.commit().unwrap();
        stats.commits += 1;
    } else {
        stats.tails += 1;
        if !synced || rng.chance(50) {
            let (size, cseed) = (pick_size(rng, 0), rng.next_u64());
            put!(
                db,
                model,
                &format!("d-{round}-sync"),
                size,
                cseed,
                Durability::Fsync
            );
            stats.puts += 1;
        }
    }
    drop(db);
}

#[test]
fn seeded_soak_of_puts_deletes_and_replaces_interleaved_with_persia_gc() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x5eed_1045_50a6);
    let rounds = env_count("PERSIA_TEST_ROUNDS", SOAK_ROUNDS);
    let started = Instant::now();
    let dir = KeptOnFailure::new();
    let path = create(dir.path());
    let mut rng = Rng(seed);
    let mut model = Model::new();
    let mut stats = SoakStats::default();
    for round in 0..rounds {
        let ctx = format!("seed {seed} round {round}");
        soak_round(&path, round, &mut rng, &mut model, &mut stats);
        let before = verify(&path, false, &format!("{ctx}: before gc"));
        let done = gc_ok(&path, &ctx);
        assert_eq!(done["held_back"], false, "{ctx}: {done}");
        assert_eq!(done["missing"], serde_json::json!([]), "{ctx}: {done}");
        assert_eq!(done["marked"]["blobs"], before.referenced, "{ctx}: {done}");
        assert_eq!(readers(&done).0, 0, "{ctx}: {done}");
        let deep = round % 4 == 3 || round + 1 == rounds;
        let after = verify(&path, deep, &format!("{ctx}: after gc"));
        assert_eq!(after.referenced, before.referenced, "{ctx}");
        match done["rewrite"]["outcome"].as_str().unwrap() {
            "done" => {
                stats.rewrites += 1;
                // No unreferenced manifest survives a rewrite, and `deleted` counts every one the pack had.
                assert_eq!(after.bound, after.referenced, "{ctx}: {after:?}");
                assert_eq!(after.orphan_chunks, 0, "{ctx}: {after:?}");
                assert_eq!(after.indexes, after.bound, "{ctx}: {after:?}");
                assert_eq!(
                    done["deleted"]["blobs"],
                    before.bound - before.referenced,
                    "{ctx}: {done} {before:?}"
                );
                stats.deleted += before.bound - before.referenced;
            }
            "below_ratio" => {
                stats.below_ratio += 1;
                // Below the default ratio nothing is dropped (SPEC §9.3): every manifest stays, the unreferenced too.
                let dead = done["rewrite"]["dead_bytes"].as_u64().unwrap();
                let frames = done["rewrite"]["frame_bytes"].as_u64().unwrap();
                assert!(2 * dead < frames, "{ctx}: {done}");
                assert_eq!(done["deleted"]["blobs"], 0, "{ctx}: {done}");
                assert_eq!(after.bound, before.bound, "{ctx}: {after:?} {before:?}");
            }
            other => panic!("{ctx}: rewrite {other}: {done}"),
        }
        check_reads(&path, &model, &ctx);
    }
    eprintln!(
        "{stats:?}, {} live documents, pack {} KiB, total {:.1?}",
        model.len(),
        std::fs::metadata(pack(&path)).unwrap().len() >> 10,
        started.elapsed()
    );
    assert!(stats.rewrites > 0, "no run rewrote the pack: {stats:?}");
}

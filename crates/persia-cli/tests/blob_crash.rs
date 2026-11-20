//! Real-file, real-process crash loop of `put(doc, blob)` (SPEC §9.5, §9.3, §8.2.1; ROADMAP 9.4.6, 9.5.3): a writer
//! child stores blobs with documents through `BlobDatabase::open` on a temp-dir file and is killed (SIGKILL) at seeded
//! points, crash after crash on the same container and pack.
//!
//! The writer child reports `opening` before `BlobDatabase::open`, `opened` after it, `start <id> <size> <cseed>
//! <durability>` before each `put` and `ack <id> <blob_id>` once it returned, and `commit <seq>` after the commits it
//! makes every few puts. Blob contents are a pure function of `(size, cseed)` (sizes 0 B, 1 B, small, `chunk - 1`,
//! `chunk`, `chunk + 1` and multi-chunk at the default 4 MiB chunk size); a third of the puts reuse a pooled content
//! seed, so duplicates (dedup at bind) cross kills. Ids are unique per child: nothing is replaced or deleted.
//!
//! The parent kills the child right after its `opening` report (a kill inside the open: the container's replay and
//! the pack's recovery and sweep) or right after a seeded report (`start`: inside a `put`, while chunks are staged or
//! bound or the record is logged; `ack`: between puts). No timer: the kill point is a report count.
//!
//! Invariants, after every kill:
//! 1. `persia verify` (the CLI binary) passes on the file as the kill left it, before any read-write open: every
//!    reference is bound and every frame's checksum is right; with `--deep` (every fourth kill and the last one)
//!    every referenced blob also hashes to its id.
//! 2. A read-only open (`ReadOnlyBlobDatabase`) shows only documents the workload started, each with the size and
//!    the `blob_id` its `put` was given and acknowledged. A document no earlier check showed (and every document,
//!    on a deep kill) reads its blob back whole, byte-identical to what its `put` was given; the others read their
//!    last byte.
//! 3. Every `Fsync` put that was acknowledged, and every document an earlier reopen showed, is present.
//! 4. A read-write reopen (`BlobDatabase::open`, which recovers the container and the pack) shows the same documents
//!    as the read-only open, their blobs read through it as in 2, and `persia verify` passes again after it.
//!
//! SIGKILL kills the process, not the machine: writes the child never synced stay in the page cache. Power loss is
//! simulated on `SimFs` by `crates/persia/tests/put_crash.rs`.
//!
//! Seeds: `PERSIA_TEST_SEED` (printed); `PERSIA_TEST_FILES` and `PERSIA_TEST_KILLS` override the number of files and
//! kills per file. A replay reproduces the schedule (kill report counts, sizes, durabilities, commit cadence), not the
//! exact instant inside a `put`. On a failure the temp dir is kept and its path printed.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::too_many_lines
)] // Tests: inputs are generated or fixed.

mod blob_support;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use blob_support::{
    COLLECTION, ChildIo, KeptOnFailure, Rng, base_seed, child_env, content, env_count, live_ids,
    parent_gone, pick_size, report, row, serial, verify_ok,
};
use persia::internal::documents::{Blob, BlobDatabase, ReadOnlyBlobDatabase};
use persia_engine::db::DatabaseOptions;
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_format::BlobRef;

#[cfg(not(feature = "slow"))]
const FILES: u64 = 1;
#[cfg(feature = "slow")]
const FILES: u64 = 4;

#[cfg(not(feature = "slow"))]
const KILLS_PER_FILE: u64 = 12;
#[cfg(feature = "slow")]
const KILLS_PER_FILE: u64 = 50;

/// The share of puts whose blob is at least one 4 MiB chunk.
const BIG_PERCENT: u64 = 8;

/// The content seeds a third of the puts reuse: duplicates of blobs earlier puts (and earlier children) stored.
const POOL: [u64; 4] = [11, 22, 33, 44];

// ---------------------------------------------------------------------------------------------------------------
// Child role
// ---------------------------------------------------------------------------------------------------------------

/// The entry point of every child process; a no-op in a normal test run.
#[test]
fn child_role() {
    let Some(env) = child_env() else {
        return;
    };
    match env.role.as_str() {
        "crash-writer" => crash_writer(&env.path, env.seed, &env.prefix),
        other => panic!("unknown child role {other}"),
    }
}

/// Puts `<prefix>-0`, `<prefix>-1`, ... each with a blob, until killed (or until the parent is gone).
fn crash_writer(path: &Path, seed: u64, prefix: &str) {
    let gone = parent_gone();
    let mut rng = Rng(seed);
    let commit_every = rng.range(0, 6); // 0: never
    report("opening");
    let mut db = BlobDatabase::open(path, DatabaseOptions::default()).unwrap();
    report("opened");
    for n in 0_u64.. {
        if gone.load(Ordering::Relaxed) {
            return;
        }
        let id = format!("{prefix}-{n}");
        let size = pick_size(&mut rng, BIG_PERCENT);
        let cseed = if rng.chance(33) {
            POOL[usize::try_from(rng.range(0, 4)).unwrap()]
        } else {
            rng.next_u64()
        };
        let (durability, tag) = if rng.chance(80) {
            (Durability::Fsync, "fsync")
        } else {
            (Durability::Buffered, "buffered")
        };
        let bytes = content(size, cseed);
        report(&format!("start {id} {size} {cseed} {tag}"));
        let blob = db
            .put(
                COLLECTION,
                &Document::new(&id),
                Blob::from_reader(&bytes[..]),
                durability,
            )
            .unwrap();
        report(&format!("ack {id} {}", blob.blob_id()));
        if commit_every > 0 && n % commit_every == commit_every - 1 {
            let seq = db.commit().unwrap();
            report(&format!("commit {seq}"));
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Parent
// ---------------------------------------------------------------------------------------------------------------

/// What the parent knows of one started put.
#[derive(Debug, Clone)]
struct Entry {
    size: u64,
    cseed: u64,
    fsync: bool,
    /// The `blob_id` its `ack` reported.
    acked: Option<String>,
    /// Shown by an earlier reopen: it must stay.
    seen: bool,
}

impl Entry {
    fn required(&self) -> bool {
        (self.fsync && self.acked.is_some()) || self.seen
    }
}

/// When the parent kills a child.
#[derive(Debug, Clone, Copy)]
enum KillAt {
    /// Right after its `opening` report.
    Opening,
    /// Right after its `n`th report after `opened` (1-based).
    Report(u64),
}

#[derive(Debug, Default)]
struct Stats {
    kills: u64,
    kills_in_open: u64,
    kills_mid_put: u64,
    acked: u64,
    bytes_acked: u64,
    commits: u64,
    documents: usize,
    check_ms: Vec<u128>,
}

fn record(msg: &str, model: &mut BTreeMap<String, Entry>, stats: &mut Stats, ctx: &str) -> bool {
    let parts: Vec<&str> = msg.split(' ').collect();
    match parts.as_slice() {
        ["opening" | "opened"] => false,
        ["start", id, size, cseed, durability] => {
            let prev = model.insert(
                (*id).to_owned(),
                Entry {
                    size: size.parse().unwrap(),
                    cseed: cseed.parse().unwrap(),
                    fsync: *durability == "fsync",
                    acked: None,
                    seen: false,
                },
            );
            assert!(prev.is_none(), "{ctx}: {id} started twice");
            true
        }
        ["ack", id, blob_id] => {
            let e = model
                .get_mut(*id)
                .unwrap_or_else(|| panic!("{ctx}: ack of {id}, never started"));
            e.acked = Some((*blob_id).to_owned());
            stats.acked += 1;
            stats.bytes_acked += e.size;
            false
        }
        ["commit", _] => {
            stats.commits += 1;
            false
        }
        _ => panic!("{ctx}: unknown report {msg:?}"),
    }
}

/// Runs one writer child and kills it at `at`; returns whether a `put` was in flight at the kill.
fn run_and_kill(
    path: &Path,
    seed: u64,
    prefix: &str,
    at: KillAt,
    model: &mut BTreeMap<String, Entry>,
    stats: &mut Stats,
    ctx: &str,
) -> bool {
    let child = ChildIo::spawn("crash-writer", path, seed, prefix, 0);
    let mut after_open = 0;
    let mut opened = false;
    let mut in_flight = false;
    loop {
        let msg = child.expect(ctx);
        let started = record(&msg, model, stats, ctx);
        if started {
            in_flight = true;
        } else if msg.starts_with("ack ") {
            in_flight = false;
        }
        if msg == "opening" && matches!(at, KillAt::Opening) {
            break;
        }
        if msg == "opened" {
            opened = true;
            continue;
        }
        if opened {
            after_open += 1;
            if let KillAt::Report(n) = at
                && after_open >= n
            {
                break;
            }
        }
    }
    for msg in child.kill() {
        if record(&msg, model, stats, ctx) {
            in_flight = true;
        } else if msg.starts_with("ack ") {
            in_flight = false;
        }
    }
    in_flight
}

/// Reads a blob whole.
type ReadBlob<'a> = &'a mut dyn FnMut(&BlobRef) -> Vec<u8>;

/// Checks one live document against the model: started, with the size and `blob_id` its put was given, and, when
/// `read` is given, with the very bytes.
fn check_document(
    model: &BTreeMap<String, Entry>,
    id: &str,
    blob: &BlobRef,
    read: Option<ReadBlob<'_>>,
    ctx: &str,
) {
    let e = model
        .get(id)
        .unwrap_or_else(|| panic!("{ctx}: phantom document {id}"));
    assert_eq!(blob.size(), e.size, "{ctx}: {id}: size");
    if let Some(acked) = &e.acked {
        assert_eq!(&blob.blob_id().to_string(), acked, "{ctx}: {id}: blob_id");
    }
    if let Some(read) = read {
        let bytes = read(blob);
        assert!(
            bytes == content(e.size, e.cseed),
            "{ctx}: {id}: blob {} reads back other bytes ({} of {})",
            blob.blob_id(),
            bytes.len(),
            e.size
        );
    }
}

/// The checks after a kill (module docs, invariants 1-4); marks every document shown as seen. A document no earlier
/// check showed is read back whole and compared byte for byte; one already checked is compared again only when
/// `deep` (and otherwise its last byte read, so its chunks are still reachable): `persia verify` covers it on every
/// kill (its frames' checksums by default, its hash against the `blob_id` the child acknowledged with `--deep`).
fn check_after_kill(
    path: &Path,
    model: &mut BTreeMap<String, Entry>,
    deep: bool,
    ctx: &str,
) -> usize {
    let depth: &[&str] = if deep { &["--deep"] } else { &[] };
    // 1. The CLI, on the file exactly as the kill left it.
    let rows = verify_ok(depth, path, &format!("{ctx}: verify after kill"));
    assert_eq!(row(&rows, "verify"), "ok", "{ctx}");

    // 2, 3. Read-only.
    let ro = ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default())
        .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"));
    let snapshot = ro.database().snapshot();
    let shown_read_only = live_ids(&snapshot, COLLECTION);
    let c = snapshot.collection(COLLECTION).unwrap();
    for id in &shown_read_only {
        let doc = c.get(id).unwrap().unwrap();
        let blob = doc
            .blob_ref()
            .unwrap_or_else(|| panic!("{ctx}: {id} has no blob"));
        let full = deep || model.get(id).is_none_or(|e| !e.seen);
        let mut read = |b: &BlobRef| {
            let mut out = Vec::new();
            ro.get_blob(b)
                .unwrap_or_else(|e| panic!("{ctx}: get_blob({id}): {e}"))
                .read_to_end(&mut out)
                .unwrap_or_else(|e| panic!("{ctx}: reading {id}'s blob: {e}"));
            out
        };
        if full {
            check_document(model, id, blob, Some(&mut read), ctx);
        } else {
            check_document(model, id, blob, None, ctx);
            if blob.size() > 0 {
                ro.get_blob_range(blob, blob.size() - 1..blob.size())
                    .unwrap_or_else(|e| panic!("{ctx}: last byte of {id}'s blob: {e}"));
            }
        }
    }
    for (id, e) in model.iter() {
        assert!(
            !e.required() || shown_read_only.contains(id),
            "{ctx}: {id} lost ({e:?})"
        );
    }
    drop(snapshot);
    drop(ro);

    // 4. Read-write: the recovery of both files.
    let db = BlobDatabase::open(path, DatabaseOptions::default())
        .unwrap_or_else(|e| panic!("{ctx}: read-write reopen: {e}"));
    let snapshot = db.log().snapshot();
    let shown_read_write = live_ids(&snapshot, COLLECTION);
    assert_eq!(
        shown_read_write, shown_read_only,
        "{ctx}: the read-write reopen shows other documents than the read-only one"
    );
    let c = snapshot.collection(COLLECTION).unwrap();
    for id in &shown_read_write {
        let doc = c.get(id).unwrap().unwrap();
        let blob = doc.blob_ref().unwrap();
        let full = deep || model.get(id).is_none_or(|e| !e.seen);
        let mut read = |b: &BlobRef| {
            let mut out = Vec::new();
            db.get_blob(b)
                .unwrap_or_else(|e| panic!("{ctx}: rw get_blob({id}): {e}"))
                .read_to_end(&mut out)
                .unwrap();
            out
        };
        if full {
            check_document(model, id, blob, Some(&mut read), ctx);
        } else {
            check_document(model, id, blob, None, ctx);
            if blob.size() > 0 {
                db.get_blob_range(blob, blob.size() - 1..blob.size())
                    .unwrap_or_else(|e| panic!("{ctx}: rw last byte of {id}'s blob: {e}"));
            }
        }
    }
    drop(snapshot);
    drop(db);
    let rows = verify_ok(depth, path, &format!("{ctx}: verify after reopen"));
    let refs = row(&rows, "references");
    assert!(
        refs.starts_with(&format!("{} to ", shown_read_write.len())),
        "{ctx}: {refs} for {} documents",
        shown_read_write.len()
    );
    for id in &shown_read_write {
        model.get_mut(id).unwrap().seen = true;
    }
    shown_read_write.len()
}

fn crash_loop(file: u64, seed: u64, kills: u64, stats: &mut Stats) {
    let dir = KeptOnFailure::new();
    let path = dir.path().join("app.persia");
    let mut db = BlobDatabase::create(&path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new(COLLECTION).build().unwrap())
        .unwrap();
    db.commit().unwrap();
    drop(db);

    let mut rng = Rng(seed);
    let mut model = BTreeMap::new();
    for kill in 0..kills {
        let child_seed = rng.next_u64();
        // Every sixth kill, and some others, lands in the open.
        let at = if kill % 6 == 5 || rng.chance(10) {
            KillAt::Opening
        } else {
            // Up to about 16 puts (start + ack + the odd commit each).
            KillAt::Report(rng.range(1, 34))
        };
        let ctx = format!("file {file} kill {kill} (child seed {child_seed}, {at:?})");
        let in_flight = run_and_kill(
            &path,
            child_seed,
            &format!("f{file}k{kill}"),
            at,
            &mut model,
            stats,
            &ctx,
        );
        stats.kills += 1;
        stats.kills_in_open += u64::from(matches!(at, KillAt::Opening));
        stats.kills_mid_put += u64::from(in_flight);
        let t = Instant::now();
        // Deep every fourth kill and on the last one.
        let deep = kill % 4 == 3 || kill + 1 == kills;
        stats.documents = check_after_kill(&path, &mut model, deep, &ctx);
        stats.check_ms.push(t.elapsed().as_millis());
    }
    let pack = std::fs::metadata(path.with_extension("persia.blobs")).map_or(0, |m| m.len());
    eprintln!(
        "file {file}: {} documents, pack {} MiB",
        model.values().filter(|e| e.seen).count(),
        pack >> 20
    );
}

#[cfg(unix)]
#[test]
fn kill_9_loop_keeps_every_put_blob_readable() {
    if child_env().is_some() {
        return;
    }
    let _serial = serial();
    let seed = base_seed(0x5eed_0946_b10b);
    let files = env_count("PERSIA_TEST_FILES", FILES);
    let kills = env_count("PERSIA_TEST_KILLS", KILLS_PER_FILE);
    let started = Instant::now();
    let mut stats = Stats::default();
    let mut rng = Rng(seed);
    for file in 0..files {
        crash_loop(file, rng.next_u64(), kills, &mut stats);
    }
    let mut ms = stats.check_ms.clone();
    ms.sort_unstable();
    let pct = |p: usize| ms.get((ms.len() * p / 100).min(ms.len().saturating_sub(1)));
    eprintln!(
        "kills {} (in open {}, mid-put {}), acked puts {} ({} MiB), commits {}, live documents at the end {}, \
         check ms p50 {:?} p99 {:?}, total {:.1?}",
        stats.kills,
        stats.kills_in_open,
        stats.kills_mid_put,
        stats.acked,
        stats.bytes_acked >> 20,
        stats.commits,
        stats.documents,
        pct(50),
        pct(99),
        started.elapsed()
    );
    assert!(stats.kills_mid_put > 0, "no kill landed inside a put");
}

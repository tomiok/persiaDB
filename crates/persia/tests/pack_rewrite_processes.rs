//! A blob pack rewrite in a real process, stopped right before its rename, then killed or let go (SPEC §9.3, §9.5,
//! §18.10; ROADMAP 9.2.10, 9.2.15, 9.5.2). The child is this test binary re-executed: it reports each step on stdout
//! and waits on stdin, so no test sleeps.
//!
//! The child opens the database read-write, marks, and runs the pack's sweep with a pin gate that reports `renaming`
//! and waits for a `go` line before it takes the pin set's lock: the point where the rewrite has written and synced
//! its new file under its temporary name (`<name>.persia.blobs.tmp.<16 hex>.<n>`) and has not renamed it yet. It holds
//! either the whole database (`db`) or, having dropped it, only the pack store (`store`: the container is free).
//!
//! Invariants, for every mode and outcome:
//! 1. While the child lives: exactly one pack temporary exists; `Container::side_files` reports it and the pack's lock
//!    in use; a second read-write open fails (the container's lock, or `PackLocked` with only the store held) and
//!    sweeps nothing; with only the store held, the container's `sweep_stale_files` (what `persia container sweep`
//!    runs) succeeds and keeps the temporary. The pack's bytes are unchanged.
//! 2. Let go, the rewrite renames its file over the pack: no temporary is left, every kept blob reads back, the
//!    dropped ones are gone.
//! 3. Killed (SIGKILL): the temporary stays and is reported free; the next read-write open or the container's sweep
//!    removes it (and never the lock file). The pack is the one before the rewrite: every kept blob reads back
//!    byte-identical, the blobs the rewrite would have dropped are still there, a deep `verify_blobs` passes, and a
//!    new GC rewrites the pack and drops them.
//!
//! Seeds: `PERSIA_TEST_SEED` (printed) sets the blob sizes and contents. On a failure the temp dir is kept and its
//! path printed.
#![cfg(unix)] // `flock` locks, SIGKILL.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::manual_assert_eq
)] // Tests: inputs are generated or fixed; `assert!(a == b)` keeps blob bytes out of a failure message.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Instant, SystemTime};

use persia::internal::blob::PutBlobOptions;
use persia::internal::documents::{Blob, BlobDatabase, Error, ReadOnlyBlobDatabase};
use persia::internal::gc::{DEFAULT_GRACE, GcOptions};
use persia::internal::pins::PinSet;
use persia::internal::verify::Scan;
use persia_blob::gc::{GcObject, PinCheck, PinGate, Rewrite, Sweep, SweepOptions};
use persia_blob::{BlobRef, BlobStore, ChunkSize};
use persia_engine::db::{DatabaseError, DatabaseOptions};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::container::{Container, Error as ContainerError, Mode};
use persia_storage::{InUse, SideFileKind};

const ROLE_ENV: &str = "PERSIA_REWRITE_CHILD_ROLE";
const PATH_ENV: &str = "PERSIA_REWRITE_CHILD_PATH";
const TAG: &str = "@@persia-rewrite ";
const COLLECTION: &str = "files";
const KEPT: usize = 7;
const DROPPED: usize = 4;

// ---------------------------------------------------------------------------------------------------------------
// Child
// ---------------------------------------------------------------------------------------------------------------

fn report(msg: &str) {
    let mut out = std::io::stdout().lock();
    out.write_all(format!("{TAG}{msg}\n").as_bytes()).unwrap();
    out.flush().unwrap();
}

/// The writer's pin set, reporting `renaming` and waiting for a `go` line before each locked step (the rewrite's
/// rename is its only one).
struct Paused(Arc<PinSet>);

impl PinGate for Paused {
    fn pinned(&self) -> Vec<GcObject> {
        PinGate::pinned(&*self.0)
    }

    fn unless_pinned<T>(
        &self,
        object: GcObject,
        delete: impl FnOnce(&dyn PinCheck) -> T,
    ) -> Option<T> {
        report("renaming");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        assert_eq!(line, "go\n");
        PinGate::unless_pinned(&*self.0, object, delete)
    }
}

/// The entry point of the child process; a no-op in a normal test run.
#[test]
fn child_role() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let path = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    let store_only = match role.as_str() {
        "rewrite-db" => false,
        "rewrite-store" => true,
        other => panic!("unknown child role {other}"),
    };
    let db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
    let mark = db.mark().unwrap();
    let store = Arc::clone(db.blobs().store());
    let gate = Paused(Arc::clone(db.blobs().pin_set()));
    let held = if store_only {
        drop(db);
        None
    } else {
        Some(db)
    };
    report("marked");
    let options = SweepOptions {
        grace: DEFAULT_GRACE,
        now: SystemTime::now(),
        rewrite_ratio: 0.0,
    };
    let swept = store.sweep(&mark, &gate, &options).unwrap();
    let done = matches!(swept.rewrite, Some(Rewrite::Done { .. }));
    report(&format!("rewritten {} {done}", swept.blobs_deleted));
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
    drop(held);
}

// ---------------------------------------------------------------------------------------------------------------
// Parent
// ---------------------------------------------------------------------------------------------------------------

struct ChildIo {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl ChildIo {
    fn spawn(role: &str, path: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
            .env(ROLE_ENV, role)
            .env(PATH_ENV, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                if out.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                // libtest may print `test child_role ... ` before the tag, without a newline.
                if let Some(rest) = line.strip_suffix('\n')
                    && let Some((_, msg)) = rest.split_once(TAG)
                    && tx.send(msg.to_owned()).is_err()
                {
                    return;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            rx,
        }
    }

    fn expect(&self, ctx: &str) -> String {
        self.rx
            .recv()
            .unwrap_or_else(|_| panic!("{ctx}: the child exited without reporting"))
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    fn kill(mut self) {
        self.child.kill().unwrap();
        let status = self.child.wait().unwrap();
        assert!(!status.success(), "the child exited by itself: {status}");
        drop(self.stdin.take());
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        assert!(status.success(), "the child failed: {status}");
    }
}

impl Drop for ChildIo {
    fn drop(&mut self) {
        // Cleanup on a failure path: the child may have exited already.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A temp dir kept (its path printed) when the test fails, removed otherwise.
struct KeptOnFailure(Option<tempfile::TempDir>);

impl Drop for KeptOnFailure {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(dir) = self.0.take()
        {
            eprintln!("kept the failed test's files in {}", dir.keep().display());
        }
    }
}

/// `SplitMix64`.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo)
    }
}

fn content(len: u64, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let len = usize::try_from(len).unwrap();
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Size class `class` (mod 7) around the 64 KiB chunk boundaries: 0, 1, small, `chunk - 1`, `chunk`, `chunk + 1`, a
/// few chunks.
fn size(class: usize, rng: &mut Rng) -> u64 {
    let chunk = u64::from(ChunkSize::MIN);
    match class % 7 {
        0 => 0,
        1 => 1,
        2 => rng.range(2, 4096),
        3 => chunk - 1,
        4 => chunk,
        5 => chunk + 1,
        _ => rng.range(2 * chunk, 5 * chunk),
    }
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(ChunkSize::MIN).unwrap(),
        ..PutBlobOptions::default()
    }
}

/// The database: `KEPT` documents with blobs (`(id, ref, bytes)`), and `DROPPED` blobs nothing references.
struct Corpus {
    kept: Vec<(String, BlobRef, Vec<u8>)>,
    dropped: Vec<BlobRef>,
}

fn create(path: &Path, rng: &mut Rng) -> Corpus {
    let mut db = BlobDatabase::create(path, DatabaseOptions::default()).unwrap();
    db.log_mut()
        .create_collection(SchemaBuilder::new(COLLECTION).build().unwrap())
        .unwrap();
    let mut kept = Vec::new();
    for i in 0..KEPT {
        let id = format!("doc-{i}");
        let len = size(i, rng);
        // Unique contents: the rewrite's dropped blobs must not be a kept one's duplicate.
        let bytes = content(len, rng.next_u64());
        let blob = db
            .put(
                COLLECTION,
                &Document::new(id.as_str()),
                Blob::from_reader(&bytes[..]).with_options(options()),
                Durability::Fsync,
            )
            .unwrap();
        kept.push((id, blob, bytes));
    }
    db.commit().unwrap();
    let mut dropped = Vec::new();
    for _ in 0..DROPPED {
        let len = rng.range(2, 3 * u64::from(ChunkSize::MIN));
        let (blob, pins) = db
            .put_blob(&content(len, rng.next_u64())[..], &options())
            .unwrap();
        drop(pins);
        dropped.push(blob);
    }
    let ids: BTreeSet<_> = kept.iter().map(|(_, b, _)| b.blob_id()).collect();
    assert_eq!(ids.len(), KEPT, "seeded contents collided");
    Corpus { kept, dropped }
}

fn pack_temporaries(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("app.persia.blobs.tmp.")
        })
        .collect();
    found.sort();
    found
}

/// The pack's temporary `temp` and lock as `Container::side_files` reports them: their `InUse`.
fn side_states(path: &Path, temp: &Path) -> (InUse, InUse) {
    let files = Container::side_files(path).unwrap();
    let find = |p: &Path, kind: SideFileKind| {
        files
            .iter()
            .find(|f| f.path == p && f.kind == kind)
            .unwrap_or_else(|| panic!("{} not listed in {files:?}", p.display()))
            .in_use
    };
    (
        find(temp, SideFileKind::Temporary),
        find(
            &path.with_file_name("app.persia.blobs.lock"),
            SideFileKind::Lock,
        ),
    )
}

/// Every kept document reads its blob back byte for byte, through a read-write open; whether each dropped blob is
/// still bound.
fn check(path: &Path, corpus: &Corpus, ctx: &str) -> Vec<bool> {
    let db = BlobDatabase::open(path, DatabaseOptions::default())
        .unwrap_or_else(|e| panic!("{ctx}: open: {e}"));
    let snapshot = db.log().snapshot();
    let c = snapshot.collection(COLLECTION).unwrap();
    for (id, blob, bytes) in &corpus.kept {
        let doc = c.get(id).unwrap().unwrap();
        assert_eq!(doc.blob_ref(), Some(blob), "{ctx}: {id}");
        let mut out = Vec::new();
        db.get_blob(blob)
            .unwrap_or_else(|e| panic!("{ctx}: get_blob({id}): {e}"))
            .read_to_end(&mut out)
            .unwrap();
        assert!(out == *bytes, "{ctx}: {id}'s blob differs");
    }
    corpus
        .dropped
        .iter()
        .map(|b| db.blobs().store().contains(b).unwrap())
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    /// The rewrite goes on and renames.
    Go,
    /// Killed; the next read-write open sweeps.
    KillThenOpen,
    /// Killed; the container's sweep (`persia container sweep`) removes the temporary.
    KillThenSweep,
}

#[test]
fn a_pack_rewrite_stopped_before_its_rename_is_kept_while_alive_and_swept_once_killed() {
    if std::env::var(ROLE_ENV).is_ok() {
        return;
    }
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_9210_5eed_u64);
    eprintln!("PERSIA_TEST_SEED={seed}");
    let mut rng = Rng(seed);
    let started = Instant::now();
    for store_only in [false, true] {
        for outcome in [Outcome::Go, Outcome::KillThenOpen, Outcome::KillThenSweep] {
            let ctx = format!("store_only {store_only}, {outcome:?}");
            let dir = KeptOnFailure(Some(tempfile::tempdir().unwrap()));
            // `open`, `side_files` and `sweep_stale_files` report the container's canonical path
            // (macOS: `/var` is a symlink to `/private/var`). The temporary `read_dir` finds is the
            // same file only when this directory is that path too.
            let dir_path = std::fs::canonicalize(dir.0.as_ref().unwrap().path()).unwrap();
            let path = dir_path.join("app.persia");
            let corpus = create(&path, &mut rng);
            let pack = path.with_file_name("app.persia.blobs");
            let lock = path.with_file_name("app.persia.blobs.lock");
            let pack_before = std::fs::read(&pack).unwrap();

            let role = if store_only {
                "rewrite-store"
            } else {
                "rewrite-db"
            };
            let mut child = ChildIo::spawn(role, &path);
            assert_eq!(child.expect(&ctx), "marked");
            assert_eq!(child.expect(&ctx), "renaming");

            // 1. The rewrite's file is written, under its temporary name, and held.
            let temps = pack_temporaries(&dir_path);
            assert_eq!(temps.len(), 1, "{ctx}: {temps:?}");
            let temp = temps[0].clone();
            assert!(std::fs::metadata(&temp).unwrap().len() > 0, "{ctx}");
            assert_eq!(side_states(&path, &temp), (InUse::Yes, InUse::Yes), "{ctx}");
            match BlobDatabase::open(&path, DatabaseOptions::default()) {
                Err(Error::Database(e))
                    if !store_only
                        && matches!(*e, DatabaseError::Container(ContainerError::Locked)) => {}
                Err(Error::Blob(persia_blob::Error::PackLocked)) if store_only => {}
                Err(e) => panic!("{ctx}: second open: {e}"),
                Ok(_) => panic!("{ctx}: a second writer opened beside the rewrite"),
            }
            match Container::open(&path, Mode::ReadWrite) {
                Ok(c) if store_only => {
                    let removed = c.sweep_stale_files().unwrap();
                    assert!(removed.is_empty(), "{ctx}: swept {removed:?}");
                }
                Err(ContainerError::Locked) if !store_only => {}
                Err(e) => panic!("{ctx}: container open: {e}"),
                Ok(_) => panic!("{ctx}: the container opened beside its writer"),
            }
            assert_eq!(
                pack_temporaries(&dir_path),
                temps,
                "{ctx}: the temporary went"
            );
            assert!(std::fs::read(&pack).unwrap() == pack_before, "{ctx}");

            match outcome {
                Outcome::Go => {
                    child.send("go");
                    assert_eq!(child.expect(&ctx), format!("rewritten {DROPPED} true"));
                    child.finish();
                    assert!(pack_temporaries(&dir_path).is_empty(), "{ctx}");
                    let bound = check(&path, &corpus, &ctx);
                    assert_eq!(bound, vec![false; DROPPED], "{ctx}: dropped blobs remain");
                    assert!(lock.exists(), "{ctx}");
                    continue;
                }
                Outcome::KillThenOpen | Outcome::KillThenSweep => child.kill(),
            }

            // 3. Killed before the rename: the temporary stays, free; the pack is the old one.
            assert_eq!(pack_temporaries(&dir_path), temps, "{ctx}");
            assert!(std::fs::read(&pack).unwrap() == pack_before, "{ctx}");
            assert_eq!(side_states(&path, &temp), (InUse::No, InUse::No), "{ctx}");
            if let Outcome::KillThenSweep = outcome {
                let c = Container::open(&path, Mode::ReadWrite).unwrap();
                assert_eq!(c.sweep_stale_files().unwrap(), temps, "{ctx}");
                drop(c);
                assert!(pack_temporaries(&dir_path).is_empty(), "{ctx}");
            }
            // The next writer's open (`check`) sweeps it, if the sweep did not.
            let bound = check(&path, &corpus, &ctx);
            assert!(pack_temporaries(&dir_path).is_empty(), "{ctx}");
            assert!(lock.exists(), "{ctx}: the lock file went");
            assert_eq!(
                bound,
                vec![true; DROPPED],
                "{ctx}: the killed rewrite dropped blobs"
            );
            let ro = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default()).unwrap();
            ro.database().verify().unwrap();
            ro.verify_blobs(Scan::Deep)
                .unwrap_or_else(|e| panic!("{ctx}: verify: {e}"));
            drop(ro);
            // A new GC finishes what the killed one began.
            let db = BlobDatabase::open(&path, DatabaseOptions::default()).unwrap();
            let report = db
                .gc(&GcOptions {
                    rewrite_ratio: 0.0,
                    ..GcOptions::default()
                })
                .unwrap();
            assert_eq!(
                report.sweep.blobs_deleted,
                u64::try_from(DROPPED).unwrap(),
                "{ctx}"
            );
            drop(db);
            assert!(pack_temporaries(&dir_path).is_empty(), "{ctx}");
            assert_eq!(check(&path, &corpus, &ctx), vec![false; DROPPED], "{ctx}");
        }
    }
    eprintln!("6 rewrites in {:.1?}", started.elapsed());
}

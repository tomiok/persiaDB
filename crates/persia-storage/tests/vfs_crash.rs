//! Crashes at every file-system step of the local backend and the container (ROADMAP 2.4.11.2; SPEC §4.5, §4.5.1,
//! §4.6, §8, §10.1.1), on the simulated file system of 2.4.11.1.
//!
//! Each scenario runs once cleanly to count its mutating calls (writes, syncs, creates, links, renames, removals,
//! directory syncs). Then, for every call `k`, it runs again with call `k` failing (without effect, or after taking
//! effect), so the "process" dies there, and checks the file system a restart finds after a crash that loses all,
//! none, or a seeded part of what was not durable (file data at sector granularity, and each directory's unsynced
//! entries as an ordered prefix):
//! 1. `LocalStorage::put` / `put_if_absent` / `delete`: the object is the old or the new version (or absent / present
//!    for a delete), never anything else, and an acknowledged call's effect always survives.
//! 2. `Container::create_in`: the path names nothing or a valid new container; an acknowledged create survives.
//! 3. `Container::compact` (temp → sync → rename → sync dir, including `CompactedNotDurable`): the path names the old
//!    or the compacted container, whole; an acknowledged compaction survives.
//! 4. The commit sequence through `open_in` (append → sync → manifest → slot write → sync): the previous or the new
//!    commit, never a mix; an acknowledged commit survives.

// Test-only code: unwraps and indexing are the point.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::container::{Commit, Container, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use persia_storage::{ErrorKind, Key, LocalStorage, PutOutcome, Storage};

const NONE: Compression = Compression::None;
const MAX: usize = 1 << 20;
const ROOT: &str = "/store";
const DB: &str = "/db/app.persia";

fn losses() -> impl Iterator<Item = CrashLoss> {
    [CrashLoss::All, CrashLoss::Nothing]
        .into_iter()
        .chain((0..6).map(CrashLoss::Seeded))
}

/// Runs `op` on a fresh `setup()` with every possible failing call, and hands each crash image to `check` with
/// what `op` returned. Also crashes after a clean run (no call fails).
fn at_every_call<R: std::fmt::Debug>(
    setup: impl Fn() -> SimFs,
    op: impl Fn(&SimFs) -> R,
    check: impl Fn(&SimFs, &R, &str),
) {
    let fs = setup();
    let start = fs.mutations();
    op(&fs);
    let calls = fs.mutations() - start;
    assert!(calls > 0, "the scenario mutates something");
    for k in 0..=calls {
        // At `k == calls` no call fails: one clean run, then the crash.
        let faults = if k == calls {
            vec![SimFault::FailAt(start + k)]
        } else {
            vec![
                SimFault::FailAt(start + k),
                SimFault::ApplyThenFail(start + k),
            ]
        };
        for fault in faults {
            let fs = setup();
            assert_eq!(fs.mutations(), start, "setup is deterministic");
            fs.set_fault(Some(fault));
            let result = op(&fs);
            for loss in losses() {
                let ctx = format!("call {k}/{calls}, {fault:?}, {loss:?}: {result:?}");
                check(&fs.crash(loss), &result, &ctx);
            }
        }
    }
}

fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

/// A durable store at `/store` holding `a/b = old`.
fn store_with_old() -> SimFs {
    let fs = SimFs::new();
    let s = LocalStorage::open_in(fs.clone(), ROOT).unwrap();
    s.put(&key("a/b"), Bytes::from_static(b"old")).unwrap();
    fs
}

/// What a restart finds at `a/b`: `None` if absent. The store must open and list only real objects.
fn object_after_restart(fs: &SimFs, ctx: &str) -> Option<Vec<u8>> {
    let s = LocalStorage::open_in(fs.clone(), ROOT).unwrap_or_else(|e| panic!("{ctx}: open: {e}"));
    let listed: Vec<String> = s
        .list(&persia_storage::KeyPrefix::new("").unwrap())
        .unwrap_or_else(|e| panic!("{ctx}: list: {e}"))
        .into_iter()
        .map(|m| m.key.as_str().to_owned())
        .collect();
    match s.get(&key("a/b")) {
        Ok(bytes) => {
            assert_eq!(listed, ["a/b"], "{ctx}");
            Some(bytes.to_vec())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            assert!(listed.is_empty(), "{ctx}: {listed:?}");
            None
        }
        Err(e) => panic!("{ctx}: get: {e}"),
    }
}

#[test]
fn local_put_is_old_or_new_and_durable_once_acknowledged() {
    at_every_call(
        store_with_old,
        |fs| {
            let s = LocalStorage::open_in(fs.clone(), ROOT).map_err(|e| e.to_string())?;
            s.put(&key("a/b"), Bytes::from_static(b"new"))
                .map_err(|e| e.to_string())
        },
        |crashed, result, ctx| {
            let got = object_after_restart(crashed, ctx);
            match result {
                Ok(()) => assert_eq!(got.as_deref(), Some(&b"new"[..]), "{ctx}"),
                Err(_) => assert!(
                    matches!(got.as_deref(), Some(b"old" | b"new")),
                    "{ctx}: {got:?}"
                ),
            }
        },
    );
}

#[test]
fn local_put_if_absent_is_absent_or_whole_and_durable_once_acknowledged() {
    at_every_call(
        || {
            let fs = SimFs::new();
            LocalStorage::open_in(fs.clone(), ROOT).unwrap();
            fs
        },
        |fs| {
            let s = LocalStorage::open_in(fs.clone(), ROOT).map_err(|e| e.to_string())?;
            s.put_if_absent(&key("a/b"), Bytes::from_static(b"new"))
                .map_err(|e| e.to_string())
        },
        |crashed, result, ctx| {
            let got = object_after_restart(crashed, ctx);
            match result {
                Ok(PutOutcome::Created) => assert_eq!(got.as_deref(), Some(&b"new"[..]), "{ctx}"),
                Ok(PutOutcome::AlreadyExists) => panic!("{ctx}: nothing else wrote it"),
                Err(_) => assert!(
                    matches!(got.as_deref(), None | Some(b"new")),
                    "{ctx}: {got:?}"
                ),
            }
        },
    );
}

#[test]
fn local_delete_is_all_or_nothing_and_durable_once_acknowledged() {
    at_every_call(
        store_with_old,
        |fs| {
            let s = LocalStorage::open_in(fs.clone(), ROOT).map_err(|e| e.to_string())?;
            s.delete(&key("a/b")).map_err(|e| e.to_string())
        },
        |crashed, result, ctx| {
            let got = object_after_restart(crashed, ctx);
            match result {
                Ok(()) => assert_eq!(got, None, "{ctx}"),
                Err(_) => assert!(
                    matches!(got.as_deref(), None | Some(b"old")),
                    "{ctx}: {got:?}"
                ),
            }
        },
    );
}

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

/// A file system with the directory `/db`, durable.
fn db_dir() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    fs
}

#[test]
fn container_create_leaves_nothing_or_a_valid_container() {
    at_every_call(
        db_dir,
        |fs| Container::create_in(vfs(fs), DB).map(drop),
        |crashed, result, ctx| {
            let found = crashed.read(Path::new(DB));
            match (result, found) {
                (Ok(()), None) => panic!("{ctx}: an acknowledged create was lost"),
                (_, None) => {}
                (_, Some(_)) => {
                    let c = Container::open_in(vfs(crashed), DB, Mode::ReadWrite)
                        .unwrap_or_else(|e| panic!("{ctx}: {e}"));
                    assert_eq!(c.superblock().seq, 0, "{ctx}");
                    assert_eq!(c.frames(), &[], "{ctx}");
                }
            }
        },
    );
}

/// A container at `/db/app.persia` with two commits (the first one's WAL frame dead after the second) and a synced
/// uncommitted WAL frame.
fn db_with_commits() -> SimFs {
    let fs = db_dir();
    let mut c = Container::create_in(vfs(&fs), DB).unwrap();
    for (i, manifest) in [b"manifest 1", b"manifest 2"].iter().enumerate() {
        let wal = c
            .append(
                FrameKind::WalChunk,
                &vec![u8::try_from(i).unwrap(); 700],
                NONE,
            )
            .unwrap();
        c.sync().unwrap();
        let m = c.append(FrameKind::Manifest, *manifest, NONE).unwrap();
        c.commit(Commit {
            manifest: m,
            wal_start: wal.offset,
            fence_token: 0,
        })
        .unwrap();
    }
    c.append(FrameKind::WalChunk, b"uncommitted", NONE).unwrap();
    c.sync().unwrap();
    fs
}

/// The committed manifest and the uncommitted payloads a restart finds at `DB`.
fn db_state(fs: &SimFs, ctx: &str) -> (u64, Vec<u8>, Vec<Vec<u8>>) {
    let c =
        Container::open_in(vfs(fs), DB, Mode::ReadWrite).unwrap_or_else(|e| panic!("{ctx}: {e}"));
    let sb = *c.superblock();
    let manifest = c.read_frame(sb.manifest_off, MAX).unwrap().payload;
    let tail = c
        .uncommitted_frames()
        .iter()
        .map(|f| c.read_frame(f.offset, MAX).unwrap().payload)
        .collect();
    (sb.seq, manifest, tail)
}

#[test]
fn container_compaction_leaves_the_old_or_the_new_container() {
    let old = db_state(&db_with_commits(), "setup");
    assert_eq!(old.0, 2);
    at_every_call(
        db_with_commits,
        |fs| {
            let mut c = Container::open_in(vfs(fs), DB, Mode::ReadWrite)?;
            c.compact(&[], |_| b"compacted".to_vec(), NONE).map(drop)
        },
        |crashed, result, ctx| {
            let got = db_state(crashed, ctx);
            assert_eq!(
                got.2, old.2,
                "{ctx}: the uncommitted WAL frame survives either way"
            );
            let new = (3, b"compacted".to_vec());
            match result {
                Ok(()) => assert_eq!((got.0, got.1), new, "{ctx}"),
                Err(_) => assert!(
                    (got.0, &got.1) == (old.0, &old.1) || (got.0, got.1.clone()) == new,
                    "{ctx}: {got:?}"
                ),
            }
        },
    );
}

#[test]
fn commit_through_the_path_api_is_the_previous_or_the_next() {
    let old = db_state(&db_with_commits(), "setup");
    at_every_call(
        db_with_commits,
        |fs| {
            // Whether the WAL frame's sync was acknowledged, and how the whole sequence ended.
            let mut synced = false;
            let result = (|| {
                let mut c = Container::open_in(vfs(fs), DB, Mode::ReadWrite)?;
                let wal = c.append(FrameKind::WalChunk, &[9; 900], NONE)?;
                c.sync()?;
                synced = true;
                let m = c.append(FrameKind::Manifest, b"manifest 3", NONE)?;
                c.commit(Commit {
                    manifest: m,
                    wal_start: wal.offset,
                    fence_token: 0,
                })
            })();
            (synced, result)
        },
        |crashed, (synced, result), ctx| {
            // A seeded loss can tear the unsynced Manifest out of order: the read-write open quarantines it (ADR-0014).
            let c = Container::open_in(vfs(crashed), DB, Mode::ReadWrite)
                .unwrap_or_else(|e| panic!("{ctx}: {e}"));
            let sb = *c.superblock();
            let manifest = c.read_frame(sb.manifest_off, MAX).unwrap().payload;
            match result {
                Ok(()) => assert_eq!((sb.seq, &manifest[..]), (3, &b"manifest 3"[..]), "{ctx}"),
                Err(_) if sb.seq == old.0 && *synced => {
                    // An acknowledged sync: the WAL frame survives as an uncommitted frame (SPEC §4.5, §8.2).
                    let found = c.uncommitted_frames().iter().any(|f| {
                        c.read_frame(f.offset, MAX)
                            .is_ok_and(|p| p.payload == [9; 900])
                    });
                    assert!(found, "{ctx}: an acknowledged WAL frame was lost");
                    assert_eq!(manifest, old.1, "{ctx}");
                }
                Err(_) => assert!(
                    (sb.seq, &manifest) == (old.0, &old.1)
                        || (sb.seq, &manifest[..]) == (3, &b"manifest 3"[..]),
                    "{ctx}: {} {manifest:?}",
                    sb.seq
                ),
            }
        },
    );
}

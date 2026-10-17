//! A read-write open whose scan from `log_end` ends in corruption (ROADMAP 2.4.10; SPEC §4.4, §4.5.1; ADR-0014).
//!
//! Invariants:
//! 1. Zeros and stale frames (`frame_seq` below the expected one) after the stop are truncated, with no quarantine.
//! 2. Other bytes are copied to a quarantine file, durably, before the container is truncated at the stop; the copy
//!    holds exactly the bytes from the stop to the old end.
//! 3. A valid frame at or above the expected `frame_seq` after the stop may be an acknowledged write: the open fails
//!    with `CorruptTail { acknowledged: Some(its offset) }` and changes nothing, unless `Repair::Quarantine`.
//! 4. Without a path (`open_on`) nothing can be quarantined: anything but zeros and stale frames fails the open.
//! 5. A crash at any step of a quarantining open never loses the tail: either the container is unchanged, or it is
//!    truncated and a quarantine file holding the whole tail is durable.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_possible_truncation
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_format::block::Compression;
use persia_format::frame::{FrameKind, ScanEnd};
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Commit, Container, ContainerFile, Error, Mode, Repair};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const NONE: Compression = Compression::None;
const DB: &str = "/db/app.persia";

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

/// A committed container with one or two synced WAL frames after `log_end`; returns the file system and the offsets
/// of the first frame and of the end of the first.
fn with_tail(two: bool) -> (SimFs, u64, u64) {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    let mut c = Container::create_in(vfs(&fs), DB).unwrap();
    let m = c.append(FrameKind::Manifest, b"m", NONE).unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: m.end(),
        fence_token: 0,
    })
    .unwrap();
    let a = c.append(FrameKind::WalChunk, &[1; 1500], NONE).unwrap();
    if two {
        c.append(FrameKind::WalChunk, &[2; 100], NONE).unwrap();
    }
    c.sync().unwrap();
    (fs, a.offset, a.end())
}

/// Overwrites `bytes` at `offset` of the container, durably, outside the container's control.
fn damage(fs: &SimFs, offset: u64, bytes: &[u8]) {
    let f = fs.open(Path::new(DB), true).unwrap();
    f.write_all_at(bytes, offset).unwrap();
    f.sync_data().unwrap();
}

fn quarantine_files(fs: &SimFs) -> Vec<(PathBuf, Vec<u8>)> {
    fs.files()
        .into_iter()
        .filter(|(p, _)| p.to_string_lossy().contains(".quarantine."))
        .collect()
}

#[test]
fn an_append_torn_out_of_order_is_quarantined_then_truncated() {
    let (fs, a, _) = with_tail(false);
    // The first sector of frame `a` never reached the disk, later ones did: no magic, non-zero bytes after it.
    damage(&fs, a, &[0; 512]);
    let before = fs.read(Path::new(DB)).unwrap();
    let tail = before[a as usize..].to_vec();

    let ro = Container::open_in(vfs(&fs), DB, Mode::ReadOnly).unwrap();
    assert!(matches!(ro.recovery().scan_end, ScanEnd::Corrupt { offset, .. } if offset == a));
    drop(ro);
    assert_eq!(
        quarantine_files(&fs),
        [],
        "a read-only open quarantines nothing"
    );

    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    assert_eq!(c.end(), a);
    assert_eq!(c.recovery().truncated_from, Some(before.len() as u64));
    let q = c.recovery().quarantined.clone().unwrap();
    assert!(
        q.to_string_lossy()
            .contains(&format!("app.persia.quarantine.{a:016x}.")),
        "{}",
        q.display()
    );
    drop(c);
    // Durable: what a crash right now keeps.
    let after = fs.crash(CrashLoss::All);
    assert_eq!(after.read(&q).unwrap(), tail);
    assert_eq!(after.read(Path::new(DB)).unwrap(), &before[..a as usize]);
    let c = Container::open_in(vfs(&after), DB, Mode::ReadWrite).unwrap();
    assert!(matches!(c.recovery().scan_end, ScanEnd::Clean { .. }));
}

#[test]
fn a_frame_that_may_be_acknowledged_is_kept_unless_repair_is_asked() {
    let (fs, a, b) = with_tail(true);
    // Frame `a` is damaged in the middle; frame `b`, synced after it, is whole and continues the run.
    damage(&fs, a + 600, &[0xEE; 8]);
    let before = fs.read(Path::new(DB)).unwrap();
    let err = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap_err();
    assert!(
        matches!(err, Error::CorruptTail { offset, acknowledged: Some(at), .. } if offset == a && at == b),
        "{err}"
    );
    assert!(
        err.to_string().contains("may hold acknowledged writes"),
        "{err}"
    );
    assert_eq!(fs.read(Path::new(DB)).unwrap(), before, "nothing changed");
    assert_eq!(quarantine_files(&fs), []);

    let c = Container::open_in_with(vfs(&fs), DB, Mode::ReadWrite, Repair::Quarantine).unwrap();
    assert_eq!(c.end(), a);
    let q = c.recovery().quarantined.clone().unwrap();
    assert_eq!(fs.read(&q).unwrap(), &before[a as usize..]);
}

#[test]
fn zeros_and_stale_frames_are_truncated_without_quarantine() {
    let (fs, a, b) = with_tail(false);
    // Frame `a` lost its first sector; what follows is zeros and a stale copy of the committed Manifest frame.
    let committed = fs.read(Path::new(DB)).unwrap();
    let manifest = committed[12288..12288 + 40].to_vec(); // the Manifest frame, 1-byte payload
    let len = committed.len() as u64;
    damage(&fs, a, &vec![0; (len - a) as usize]);
    damage(&fs, b, &manifest);
    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    assert_eq!(c.end(), a);
    assert_eq!(c.recovery().quarantined, None);
    assert_eq!(quarantine_files(&fs), []);
}

#[test]
fn without_a_path_nothing_is_quarantined() {
    let (fs, a, _) = with_tail(false);
    damage(&fs, a, &[0; 512]);
    let bytes = fs.read(Path::new(DB)).unwrap();
    let file = SimFile::from_bytes(bytes.clone());
    let err = Container::open_on(file.clone(), Mode::ReadWrite).unwrap_err();
    assert!(
        matches!(
            err,
            Error::CorruptTail {
                acknowledged: None,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(file.bytes(), bytes);
}

#[test]
fn a_crash_while_quarantining_never_loses_the_tail() {
    let setup = || {
        let (fs, a, _) = with_tail(false);
        damage(&fs, a, &[0; 512]);
        (fs, a)
    };
    let (fs, _) = setup();
    let start = fs.mutations();
    Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    let calls = fs.mutations() - start;
    for k in 0..=calls {
        for fault in [
            SimFault::FailAt(start + k),
            SimFault::ApplyThenFail(start + k),
        ] {
            let (fs, a) = setup();
            // Per run: every container gets its own random `db_uuid`.
            let before = fs.read(Path::new(DB)).unwrap();
            let tail = before[a as usize..].to_vec();
            fs.set_fault(Some(fault));
            let result = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).map(drop);
            for loss in [
                CrashLoss::All,
                CrashLoss::Nothing,
                CrashLoss::Seeded(k),
                CrashLoss::Seeded(k + 99),
            ] {
                let ctx = format!("call {k}/{calls} {fault:?} {loss:?}: {result:?}");
                let crashed = fs.crash(loss);
                let container = crashed.read(Path::new(DB)).unwrap();
                let kept = quarantine_files(&crashed)
                    .into_iter()
                    .any(|(_, q)| q == tail);
                if container.len() as u64 == a {
                    assert!(kept, "{ctx}: truncated without a durable quarantine copy");
                } else {
                    assert!(
                        container == before,
                        "{ctx}: neither the old file nor truncated"
                    );
                }
                // Whatever happened, the next open succeeds.
                Container::open_in(vfs(&crashed), DB, Mode::ReadWrite)
                    .unwrap_or_else(|e| panic!("{ctx}: {e}"));
            }
        }
    }
}

#[test]
fn a_failed_quarantine_copy_leaves_no_partial_file_and_the_container_unchanged() {
    let (fs, a, _) = with_tail(false);
    damage(&fs, a, &[0; 512]);
    let before = fs.read(Path::new(DB)).unwrap();
    // The open's calls: lock-free recovery reads nothing mutating until the quarantine file is created (call 0 from
    // here), then its first write (call 1) fails like a full disk would.
    let start = fs.mutations();
    fs.set_fault(Some(SimFault::FailAt(start + 1)));
    let err = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
    fs.set_fault(None);
    let after = fs.crash(CrashLoss::Nothing);
    assert_eq!(after.read(Path::new(DB)).unwrap(), before);
    // The removal of the partial copy failed too (the process is "dead" after a fault), so a restart may still find
    // it; a later open must not be blocked by it and creates its own copy.
    let c = Container::open_in(vfs(&after), DB, Mode::ReadWrite).unwrap();
    assert_eq!(c.end(), a);
    let q = c.recovery().quarantined.clone().unwrap();
    assert_eq!(after.read(&q).unwrap(), &before[a as usize..]);
}

//! Committed images and backups (SPEC §4.8; ROADMAP 10.1.8; ADR-0067): an image is exactly one commit of its file,
//! whatever the file gets afterwards, and `write_backup_in` leaves, at every crash point, a prefix of its files
//! linked, each complete, and after a failure the process survives nothing it made.
//!
//! Invariants:
//! 1. A copy has the image's header and commit in both slots, then the bytes `12288..log_end` of the file the commit
//!    was made in: nothing after `log_end`, nothing written or renamed over the path later.
//! 2. A destination that exists fails the backup before anything is written.
//! 3. A failure after a link removes what was linked, and every temporary name.
//! 4. For every fault point, fault kind and crash loss, the crashed file system holds, under the destinations, a
//!    prefix of the files in the order given, each byte-identical to a clean backup's.

// Test-only code: unwraps, indexing and casts are the point.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

use std::path::Path;
use std::sync::Arc;

use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_format::superblock::FIRST_FRAME_OFFSET;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{
    Commit, CommittedImage, Container, ContainerFile, Error, Mode, write_backup_in,
};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const NONE: Compression = Compression::None;
const F: usize = FIRST_FRAME_OFFSET as usize;

/// A container with two commits and WAL frames after the second, as bytes: `(bytes, log_end of the last commit)`.
fn source(uuid: u8) -> (Vec<u8>, u64) {
    let file = SimFile::new();
    let mut c = Container::create_on(file.clone(), [uuid; 16]).unwrap();
    for round in 0..2_u8 {
        let wal = c.append(FrameKind::WalChunk, &[round; 300], NONE).unwrap();
        c.sync().unwrap();
        let manifest = c
            .append(FrameKind::Manifest, &[round + 10; 40], NONE)
            .unwrap();
        c.commit(Commit {
            manifest,
            wal_start: wal.offset,
            fence_token: 0,
        })
        .unwrap();
    }
    // Acknowledged after the last commit: never in a copy.
    c.append(FrameKind::WalChunk, &[99; 500], NONE).unwrap();
    c.sync().unwrap();
    let log_end = c.superblock().log_end;
    drop(c);
    let bytes = file.bytes();
    assert!(bytes.len() as u64 > log_end);
    (bytes, log_end)
}

/// What a copy of `bytes` committed to `log_end` must be: the prefix as read, with the active slot in both places,
/// then the committed frames.
fn expected_copy(bytes: &[u8], log_end: u64) -> Vec<u8> {
    let c = Container::open_on(SimFile::from_bytes(bytes.to_vec()), Mode::ReadOnly).unwrap();
    let mut out = prefix_of(&c);
    out.extend_from_slice(&bytes[F..log_end as usize]);
    out
}

/// The prefix a copy of `c`'s commit starts with: its header, then its active slot twice.
fn prefix_of<G: ContainerFile>(c: &Container<G>) -> Vec<u8> {
    let mut w = persia_format::Writer::new();
    c.header().encode(&mut w).unwrap();
    c.superblock().encode(&mut w).unwrap();
    c.superblock().encode(&mut w).unwrap();
    w.into_inner()
}

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

/// A file system with `/d/src.persia` and `/d/src.pack` (two containers standing for a database's two files),
/// durable, and their images.
fn sources(a: &[u8], b: &[u8]) -> (SimFs, CommittedImage<SimFsFile>, CommittedImage<SimFsFile>) {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/d")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    for (name, bytes) in [("/d/src.pack", b), ("/d/src.persia", a)] {
        let f = fs.create_new(Path::new(name)).unwrap();
        f.write_all_at(bytes, 0).unwrap();
        f.sync_all().unwrap();
    }
    fs.sync_dir(Path::new("/d")).unwrap();
    let open = |name: &str| {
        Container::open_in(vfs(&fs), name, Mode::ReadOnly)
            .unwrap()
            .committed_image()
            .unwrap()
    };
    let pack = open("/d/src.pack");
    let container = open("/d/src.persia");
    (fs, container, pack)
}

const DEST: &str = "/d/backup.persia";
const DEST_PACK: &str = "/d/backup.persia.blobs";

#[test]
fn a_copy_is_one_commit_and_nothing_after_it() {
    let (bytes, log_end) = source(1);
    let (fs, container, _) = sources(&bytes, &source(2).0);
    assert_eq!(container.len(), log_end);
    assert!(!container.is_empty());
    let target = fs.create_new(Path::new("/d/copy")).unwrap();
    container.write_to(&target).unwrap();
    let copy = fs.read(Path::new("/d/copy")).unwrap();
    assert_eq!(copy, expected_copy(&bytes, log_end));
    let opened = Container::open_in(vfs(&fs), "/d/copy", Mode::ReadOnly).unwrap();
    let original = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadOnly).unwrap();
    assert_eq!(opened.superblock(), original.superblock());
    assert!(
        opened.uncommitted_frames().is_empty(),
        "no WAL after the commit"
    );
    assert_eq!(opened.end(), log_end);
}

#[test]
fn a_copy_reads_the_file_it_was_taken_from_not_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut c = Container::create(&path).unwrap();
    let wal = c.append(FrameKind::WalChunk, &[1; 200], NONE).unwrap();
    c.sync().unwrap();
    let manifest = c.append(FrameKind::Manifest, &[2; 40], NONE).unwrap();
    c.commit(Commit {
        manifest,
        wal_start: wal.offset,
        fence_token: 0,
    })
    .unwrap();
    let image = c.committed_image().unwrap();
    let before = std::fs::read(&path).unwrap();
    // A later commit, then another file renamed over the path (as a compaction does): neither is in the copy.
    let manifest = c.append(FrameKind::Manifest, &[3; 40], NONE).unwrap();
    c.commit(Commit {
        manifest,
        wal_start: manifest.offset,
        fence_token: 0,
    })
    .unwrap();
    drop(c);
    let other = dir.path().join("other.persia");
    drop(Container::create(&other).unwrap());
    std::fs::rename(&other, &path).unwrap();

    let dest = dir.path().join("copy.persia");
    let target = std::fs::File::create_new(&dest).unwrap();
    image.write_to(&target).unwrap();
    let log_end = image.len();
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        expected_copy(&before, log_end)
    );
    assert_eq!(
        Container::open(&dest, Mode::ReadOnly)
            .unwrap()
            .superblock()
            .seq,
        1
    );
}

#[test]
fn an_image_checks_its_parts() {
    let (bytes, log_end) = source(1);
    let file = SimFile::from_bytes(bytes.clone());
    let err = CommittedImage::new(vec![0; 10], file.clone(), log_end).unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    let err = CommittedImage::new(vec![0; F], file.clone(), FIRST_FRAME_OFFSET - 8).unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    // A file shorter than its commit: a committed frame is missing.
    let short = SimFile::from_bytes(bytes[..log_end as usize - 8].to_vec());
    let err = CommittedImage::new(vec![0; F], short, log_end).unwrap_err();
    assert!(err.is_corruption(), "{err:?}");
    // An image of only the prefix is empty; a copy goes into an empty file only.
    let empty = CommittedImage::new(vec![0; F], file.clone(), FIRST_FRAME_OFFSET).unwrap();
    assert!(empty.is_empty());
    let image = CommittedImage::new(vec![0; F], file, log_end).unwrap();
    let full = SimFile::from_bytes(vec![1]);
    assert!(matches!(
        image.write_to(&full).unwrap_err(),
        Error::InvalidInput(_)
    ));
}

#[test]
fn a_backup_writes_every_file_and_leaves_no_temporary_name() {
    let (a, a_end) = source(1);
    let (b, b_end) = source(2);
    let (fs, container, pack) = sources(&a, &b);
    write_backup_in(
        &fs,
        &[(Path::new(DEST_PACK), &pack), (Path::new(DEST), &container)],
    )
    .unwrap();
    // Durable: a crash that loses everything unsynced keeps both.
    let crashed = fs.crash(CrashLoss::All);
    assert_eq!(
        crashed.read(Path::new(DEST)).unwrap(),
        expected_copy(&a, a_end)
    );
    assert_eq!(
        crashed.read(Path::new(DEST_PACK)).unwrap(),
        expected_copy(&b, b_end)
    );
    // No temporary name is left, durably: the removals were synced too.
    for fs in [fs.clone(), crashed] {
        let names: Vec<_> = fs.files().into_iter().map(|(p, _)| p).collect();
        assert_eq!(
            names,
            [DEST, DEST_PACK, "/d/src.pack", "/d/src.persia"].map(std::path::PathBuf::from)
        );
    }
}

#[cfg(unix)]
#[test]
fn a_destination_that_cannot_be_looked_up_fails_before_anything_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let c = Container::create(&path).unwrap();
    let image = c.committed_image().unwrap();
    // Under a regular file: not "not found" (on Unix), an error of the lookup itself, before any write.
    let dest = path.join("copy.persia");
    let err =
        write_backup_in(&persia_storage::vfs::RealFs, &[(dest.as_path(), &image)]).unwrap_err();
    assert!(
        matches!(
            err,
            Error::Io {
                op: "look for the backup's destination",
                ..
            }
        ),
        "{err:?}"
    );
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["app.persia"]);
}

#[test]
fn a_destination_that_exists_fails_before_anything_is_written() {
    let (a, _) = source(1);
    let (b, _) = source(2);
    for taken in [DEST, DEST_PACK] {
        let (fs, container, pack) = sources(&a, &b);
        fs.create_new(Path::new(taken)).unwrap();
        let before = fs.mutations();
        let err = write_backup_in(
            &fs,
            &[(Path::new(DEST_PACK), &pack), (Path::new(DEST), &container)],
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::AlreadyExists { path } if path == Path::new(taken)),
            "{err:?}"
        );
        assert_eq!(fs.mutations(), before, "nothing written");
        assert_eq!(fs.read(Path::new(taken)).unwrap(), Vec::<u8>::new());
    }
}

#[test]
fn a_failure_after_a_link_removes_what_was_linked() {
    let (a, _) = source(1);
    let (b, _) = source(2);
    let (fs, container, pack) = sources(&a, &b);
    // The same name twice: both are free when checked, so the second link is the one that finds it taken.
    let err = write_backup_in(
        &fs,
        &[(Path::new(DEST), &pack), (Path::new(DEST), &container)],
    )
    .unwrap_err();
    assert!(matches!(err, Error::AlreadyExists { .. }), "{err:?}");
    let names: Vec<_> = fs.files().into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        names,
        ["/d/src.pack", "/d/src.persia"].map(std::path::PathBuf::from),
        "the first link and both temporary files are gone"
    );
    // And durably so.
    assert!(fs.crash(CrashLoss::Nothing).read(Path::new(DEST)).is_none());
}

/// Invariant 4.
#[test]
fn a_crash_at_any_step_leaves_a_prefix_of_complete_files() {
    let (a, a_end) = source(1);
    let (b, b_end) = source(2);
    let want_container = expected_copy(&a, a_end);
    let want_pack = expected_copy(&b, b_end);
    let calls = {
        let (fs, container, pack) = sources(&a, &b);
        let start = fs.mutations();
        write_backup_in(
            &fs,
            &[(Path::new(DEST_PACK), &pack), (Path::new(DEST), &container)],
        )
        .unwrap();
        fs.mutations() - start
    };
    assert!(calls >= 10, "{calls}");
    let mut seen = [0_u32; 3];
    for n in 0..calls {
        for apply in [false, true] {
            let losses = [
                CrashLoss::All,
                CrashLoss::Nothing,
                CrashLoss::Seeded(n),
                CrashLoss::Seeded(n + 1000),
            ];
            for loss in losses {
                let (fs, container, pack) = sources(&a, &b);
                let at = fs.mutations() + n;
                fs.set_fault(Some(if apply {
                    SimFault::ApplyThenFail(at)
                } else {
                    SimFault::FailAt(at)
                }));
                let result = write_backup_in(
                    &fs,
                    &[(Path::new(DEST_PACK), &pack), (Path::new(DEST), &container)],
                );
                assert!(result.is_err(), "fault at {n} (apply {apply}) must fail");
                let crashed = fs.crash(loss);
                let got_pack = crashed.read(Path::new(DEST_PACK));
                let got_container = crashed.read(Path::new(DEST));
                let ctx = format!("fault at {n}, apply {apply}, {loss:?}");
                if let Some(p) = &got_pack {
                    assert_eq!(p, &want_pack, "a partial pack: {ctx}");
                }
                if let Some(c) = &got_container {
                    assert_eq!(c, &want_container, "a partial container: {ctx}");
                    assert!(got_pack.is_some(), "a container without its pack: {ctx}");
                }
                seen[usize::from(got_pack.is_some()) + usize::from(got_container.is_some())] += 1;
            }
        }
    }
    // Every outcome happened somewhere: nothing, the pack alone, both.
    assert!(seen.iter().all(|&k| k > 0), "{seen:?}");
}

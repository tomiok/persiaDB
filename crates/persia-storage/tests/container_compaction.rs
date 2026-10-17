//! Container compaction (ROADMAP 2.4.5; SPEC §4.6): space accounting, the rewritten file, the rename over the
//! container, and crashes while compacting.

// Test-only code: unwraps, indexing and casts are the point.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::single_range_in_vec_init // a list of one live frame is the point
)]

use std::ops::Range;
use std::path::Path;

use persia_format::Reader;
use persia_format::block::Compression;
use persia_format::frame::{Frame, FrameKind};
use persia_format::superblock::FIRST_FRAME_OFFSET;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{
    Commit, Container, ContainerFile, Error, FrameRef, Mode, Relocation, SpaceUsage,
};

const NONE: Compression = Compression::None;
const ZSTD: Compression = Compression::Zstd { level: 3 };
const MAX: usize = 1 << 20;
const F: u64 = FIRST_FRAME_OFFSET;

fn range(f: FrameRef) -> Range<u64> {
    f.offset..f.end()
}

/// A manifest payload naming the live segment: `"manifest: segment at <offset>"`.
fn manifest_for(segment: u64) -> Vec<u8> {
    format!("manifest: segment at {segment}").into_bytes()
}

/// What a test container holds, by frame.
struct Built<F> {
    c: Container<F>,
    /// The segment the latest manifest references.
    live: FrameRef,
    /// A replaced segment.
    dead: FrameRef,
    /// Committed WAL frames still needed (from `wal_start`).
    wal: Vec<FrameRef>,
    /// A synced WAL frame after `log_end`.
    tail: FrameRef,
}

/// Two commits: the first with segment `dead` and a consumed WAL frame, the second replacing it with `live`; then
/// WAL frames still needed, a third commit, and an acknowledged WAL frame after it.
fn build<F: ContainerFile>(mut c: Container<F>) -> Built<F> {
    let consumed = c
        .append(FrameKind::WalChunk, b"consumed wal", NONE)
        .unwrap();
    let dead = c.append(FrameKind::Segment, &[1; 3000], NONE).unwrap();
    c.sync().unwrap();
    let m = c
        .append(FrameKind::Manifest, &manifest_for(dead.offset), NONE)
        .unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: consumed.offset,
        fence_token: 9,
    })
    .unwrap();

    let live = c.append(FrameKind::Segment, &[2; 4000], ZSTD).unwrap();
    c.sync().unwrap();
    let m = c
        .append(FrameKind::Manifest, &manifest_for(live.offset), NONE)
        .unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: m.end(),
        fence_token: 9,
    })
    .unwrap();

    let w1 = c.append(FrameKind::WalChunk, b"wal 1", NONE).unwrap();
    let w2 = c
        .append(FrameKind::WalChunk, &b"wal 2 ".repeat(100), ZSTD)
        .unwrap();
    c.sync().unwrap();
    let m = c
        .append(FrameKind::Manifest, &manifest_for(live.offset), NONE)
        .unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: w1.offset,
        fence_token: 9,
    })
    .unwrap();
    let tail = c
        .append(FrameKind::WalChunk, b"acknowledged", NONE)
        .unwrap();
    c.sync().unwrap();
    Built {
        c,
        live,
        dead,
        wal: vec![w1, w2, m],
        tail,
    }
}

fn sim() -> Built<SimFile> {
    build(Container::create_on(SimFile::new(), [5; 16]).unwrap())
}

/// Builds the new manifest the way an engine would: from where the live segment moved.
fn new_manifest(live: FrameRef) -> impl FnOnce(&Relocation) -> Vec<u8> {
    move |r: &Relocation| manifest_for(r.get(live.offset).unwrap().offset)
}

fn payload<F: ContainerFile>(c: &Container<F>, offset: u64) -> Vec<u8> {
    c.read_frame(offset, MAX).unwrap().payload
}

fn stored(file: &SimFile, f: FrameRef) -> Vec<u8> {
    let bytes = file.bytes();
    let frame = Frame::decode(&mut Reader::with_base_offset(
        &bytes[f.offset as usize..f.end() as usize],
        f.offset,
    ))
    .unwrap();
    frame.stored_payload().to_vec()
}

// ---------------------------------------------------------------------------------------------------------------
// Space accounting (ROADMAP 2.4.5.1)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn space_usage_counts_live_frames_and_everything_from_wal_start() {
    let b = sim();
    let sb = *b.c.superblock();
    let usage = b.c.space_usage(&[range(b.live)]).unwrap();
    assert_eq!(usage.total, b.c.end());
    assert_eq!(usage.live, F + b.live.len + (b.c.end() - sb.wal_start));
    // Dead: the consumed WAL frame, the replaced segment, two old manifests.
    assert!(usage.dead() >= b.dead.len);
    // Duplicates and order do not matter.
    let again = b.c.space_usage(&[range(b.live), range(b.live)]).unwrap();
    assert_eq!(again, usage);
    let all_dead = b.c.space_usage(&[]).unwrap();
    assert_eq!(all_dead.dead(), usage.dead() + b.live.len);
}

#[test]
fn space_usage_rejects_impossible_ranges() {
    let b = sim();
    let wal_start = b.c.superblock().wal_start;
    let bad = [
        vec![F - 8..F + 40],                                  // inside the prefix
        vec![wal_start..wal_start + 40],                      // not below wal_start
        vec![b.live.offset..b.live.offset],                   // empty
        vec![range(b.dead), b.dead.offset + 8..b.dead.end()], // overlapping
        vec![b.live.offset..wal_start + 8],
    ];
    for live in bad {
        let err = b.c.space_usage(&live).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{live:?}: {err}");
    }
}

#[test]
fn compaction_is_due_at_the_ratio() {
    let usage = SpaceUsage {
        total: 100,
        live: 60,
    };
    assert_eq!(usage.dead(), 40);
    assert!(usage.is_due(40) && usage.is_due(0) && !usage.is_due(41));
    let empty = SpaceUsage { total: 0, live: 0 };
    assert!(!empty.is_due(0), "nothing dead is never due");
    let huge = SpaceUsage {
        total: u64::MAX,
        live: 0,
    };
    assert!(huge.is_due(100));
    assert_eq!(SpaceUsage { total: 5, live: 9 }.dead(), 0);
}

// ---------------------------------------------------------------------------------------------------------------
// The rewritten file (SPEC §4.6)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn compacted_file_keeps_live_frames_wal_and_tail() {
    let mut b = sim();
    let old_sb = *b.c.superblock();
    let old_next = b.c.next_frame_seq();
    let old_header = *b.c.header();
    let old_bytes = b.c.file().bytes();
    let old_mutations = b.c.file().mutations();

    let (new, relocation) =
        b.c.compact_into(SimFile::new(), &[range(b.live)], new_manifest(b.live), NONE)
            .unwrap();
    assert_eq!(
        b.c.file().bytes(),
        old_bytes,
        "the old file is never written"
    );
    assert_eq!(b.c.file().mutations(), old_mutations);

    // Order: the live segment, the WAL range (its old Manifest included), the new Manifest, then the tail.
    let moved: Vec<(u64, u64)> = relocation
        .iter()
        .map(|(o, n)| (o.offset, n.offset))
        .collect();
    let mut old_order = vec![b.live.offset];
    old_order.extend(b.wal.iter().map(|f| f.offset));
    old_order.push(b.tail.offset);
    assert_eq!(moved.iter().map(|m| m.0).collect::<Vec<_>>(), old_order);
    assert_eq!(relocation.len(), 5);
    assert_eq!(relocation.get(b.dead.offset), None);
    let new_live = relocation.get(b.live.offset).unwrap();
    assert_eq!(new_live.offset, F, "the first live frame starts the file");

    // Renumbered on from the old next value, contiguously; kinds and stored payloads unchanged.
    for (i, (old, n)) in relocation.iter().enumerate() {
        assert_eq!(n.kind, old.kind);
        assert_eq!(n.len, old.len);
        let expect_seq = old_next + i as u64 + u64::from(old.offset == b.tail.offset);
        assert_eq!(n.frame_seq, expect_seq, "{old:?}");
        assert_eq!(stored(new.file(), n), stored(b.c.file(), old));
    }

    let check = |c: &Container<SimFile>| {
        let sb = *c.superblock();
        assert_eq!(sb.seq, old_sb.seq + 1);
        assert_eq!(sb.fence_token, 9);
        assert_eq!(
            sb.wal_start,
            relocation.get(b.wal[0].offset).unwrap().offset
        );
        assert_eq!(payload(c, sb.manifest_off), manifest_for(new_live.offset));
        assert_eq!(payload(c, new_live.offset), vec![2; 4000]);
        assert_eq!(
            payload(c, relocation.get(b.wal[0].offset).unwrap().offset),
            b"wal 1"
        );
        assert_eq!(
            payload(c, relocation.get(b.wal[1].offset).unwrap().offset),
            b"wal 2 ".repeat(100)
        );
        let tail = relocation.get(b.tail.offset).unwrap();
        assert_eq!(c.uncommitted_frames(), [tail]);
        assert_eq!(payload(c, tail.offset), b"acknowledged");
        assert_eq!(c.next_frame_seq(), old_next + 6);
        assert_eq!(c.header().db_uuid, old_header.db_uuid);
        assert_eq!(c.end(), c.file().bytes().len() as u64);
        let usage = c.space_usage(&[range(new_live)]).unwrap();
        assert_eq!(usage.dead(), 0);
    };
    check(&new);
    assert_eq!(
        new.file().durable_bytes(),
        new.file().bytes(),
        "synced before it is returned"
    );
    // Both slots hold the commit.
    let bytes = new.file().bytes();
    assert_eq!(bytes[4096..8192], bytes[8192..12288]);
    // A reopen recovers the same state, and the new file accepts commits.
    let mut r = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadWrite).unwrap();
    check(&r);
    assert_eq!(r.frames(), new.frames());
    let m = r
        .append(FrameKind::Manifest, b"after compaction", NONE)
        .unwrap();
    let wal_start = r.superblock().wal_start;
    r.commit(Commit {
        manifest: m,
        wal_start,
        fence_token: 9,
    })
    .unwrap();
    let r = Container::open_on(SimFile::from_bytes(r.file().bytes()), Mode::ReadOnly).unwrap();
    assert_eq!(
        payload(&r, r.superblock().manifest_off),
        b"after compaction"
    );
}

#[test]
fn empty_wal_range_puts_wal_start_after_the_manifest() {
    let mut c = Container::create_on(SimFile::new(), [5; 16]).unwrap();
    let seg = c.append(FrameKind::Segment, b"segment", NONE).unwrap();
    c.sync().unwrap();
    let m = c.append(FrameKind::Manifest, b"m", NONE).unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: m.end(),
        fence_token: 0,
    })
    .unwrap();
    let (new, _) = c
        .compact_into(SimFile::new(), &[range(seg)], |_| b"m2".to_vec(), NONE)
        .unwrap();
    let sb = *new.superblock();
    assert_eq!(
        (sb.wal_start, sb.log_end),
        (sb.log_end, sb.manifest_off + sb.manifest_len)
    );
    assert_eq!(new.frames().len(), 0);
    let r = Container::open_on(SimFile::from_bytes(new.file().bytes()), Mode::ReadOnly).unwrap();
    assert_eq!(payload(&r, sb.manifest_off), b"m2");
}

#[test]
fn a_new_container_compacts_to_a_committed_one() {
    let mut c = Container::create_on(SimFile::new(), [5; 16]).unwrap();
    let (new, relocation) = c
        .compact_into(SimFile::new(), &[], |_| b"first".to_vec(), NONE)
        .unwrap();
    assert!(relocation.is_empty());
    let sb = *new.superblock();
    assert_eq!((sb.seq, sb.next_frame_seq), (1, 2));
    Container::open_on(SimFile::from_bytes(new.file().bytes()), Mode::ReadWrite).unwrap();
}

#[test]
fn unsynced_appends_are_synced_and_copied() {
    let mut b = sim();
    let extra =
        b.c.append(FrameKind::WalChunk, b"not yet synced", NONE)
            .unwrap();
    assert!(b.c.has_unsynced());
    let (new, relocation) =
        b.c.compact_into(SimFile::new(), &[range(b.live)], new_manifest(b.live), NONE)
            .unwrap();
    assert!(!b.c.has_unsynced());
    let moved = relocation.get(extra.offset).unwrap();
    assert!(new.uncommitted_frames().contains(&moved));
}

#[test]
fn bad_compaction_requests_change_nothing() {
    let mut b = sim();
    let old_bytes = b.c.file().bytes();
    let cases: Vec<Vec<Range<u64>>> = vec![
        vec![b.live.offset..b.live.end() - 8], // not the whole frame
        vec![b.live.offset + 8..b.live.end()], // not a frame start
        vec![range(b.dead), b.dead.offset..b.live.end()],
    ];
    for live in cases {
        let err =
            b.c.compact_into(SimFile::new(), &live, |_| Vec::new(), NONE)
                .unwrap_err();
        assert!(
            matches!(err, Error::InvalidInput(_)) || err.is_corruption(),
            "{live:?}: {err}"
        );
    }
    let err =
        b.c.compact_into(SimFile::from_bytes(vec![0]), &[], |_| Vec::new(), NONE)
            .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    assert_eq!(b.c.file().bytes(), old_bytes);
    // Still writable.
    b.c.append(FrameKind::WalChunk, b"still fine", NONE)
        .unwrap();
    b.c.sync().unwrap();
}

#[test]
fn a_corrupt_live_frame_stops_compaction() {
    let b = sim();
    let mut bytes = b.c.file().bytes();
    bytes[b.live.offset as usize + 40] ^= 1; // below wal_start: recovery does not read it
    let mut c = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadWrite).unwrap();
    let err = c
        .compact_into(SimFile::new(), &[range(b.live)], |_| Vec::new(), NONE)
        .unwrap_err();
    assert!(err.is_corruption(), "{err}");
}

#[test]
fn read_only_and_poisoned_handles_do_not_compact() {
    let b = sim();
    let mut ro =
        Container::open_on(SimFile::from_bytes(b.c.file().bytes()), Mode::ReadOnly).unwrap();
    let err = ro
        .compact_into(SimFile::new(), &[], |_| Vec::new(), NONE)
        .unwrap_err();
    assert!(matches!(err, Error::ReadOnly { .. }), "{err}");

    let file = SimFile::from_bytes(b.c.file().bytes());
    let mut c = Container::open_on(file.clone(), Mode::ReadWrite).unwrap();
    file.set_fault(Some(SimFault::FailAt(file.mutations())));
    assert!(c.append(FrameKind::WalChunk, b"x", NONE).is_err());
    let err = c
        .compact_into(SimFile::new(), &[], |_| Vec::new(), NONE)
        .unwrap_err();
    assert!(matches!(err, Error::Poisoned), "{err}");
}

// ---------------------------------------------------------------------------------------------------------------
// Crashes while compacting (ROADMAP 2.4.5.2)
// ---------------------------------------------------------------------------------------------------------------

/// The old file is never written, so a crash at any point of writing the new one leaves the old container exactly
/// as it was; and once `compact_into` returns, the new file is durable as a whole. The rename that publishes it is
/// atomic (tested on real files below; its crash windows are ROADMAP 2.4.11.2).
#[test]
fn crash_while_writing_the_new_file_leaves_the_old_container() {
    let template = sim();
    let old_bytes = template.c.file().bytes();
    let calls = {
        let mut b = sim();
        let target = SimFile::new();
        b.c.compact_into(target.clone(), &[range(b.live)], new_manifest(b.live), NONE)
            .unwrap();
        target.mutations()
    };
    assert!(calls >= 7, "frames, header, sync: {calls}");
    let old_state = {
        let c = Container::open_on(SimFile::from_bytes(old_bytes.clone()), Mode::ReadOnly).unwrap();
        (*c.superblock(), c.frames().to_vec())
    };
    for n in 0..calls {
        for fault in [SimFault::FailAt(n), SimFault::ApplyThenFail(n)] {
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(n)] {
                let ctx = format!("{fault:?} {loss:?}");
                let mut b = sim();
                let target = SimFile::new();
                target.set_fault(Some(fault));
                let result =
                    b.c.compact_into(target.clone(), &[range(b.live)], new_manifest(b.live), NONE);
                assert!(result.is_err(), "{ctx}");
                // The old container: untouched, still writable by this handle, and recovers as before.
                assert_eq!(b.c.file().bytes(), old_bytes, "{ctx}");
                let r = Container::open_on(b.c.file().crash(loss), Mode::ReadWrite).unwrap();
                assert_eq!((*r.superblock(), r.frames().to_vec()), old_state, "{ctx}");
                b.c.append(FrameKind::WalChunk, b"x", NONE).unwrap();
            }
        }
    }
    // A completed compaction survives losing everything unsynced.
    let mut b = sim();
    let target = SimFile::new();
    let (new, _) =
        b.c.compact_into(target.clone(), &[range(b.live)], new_manifest(b.live), NONE)
            .unwrap();
    let r = Container::open_on(target.crash(CrashLoss::All), Mode::ReadOnly).unwrap();
    assert_eq!(
        (*r.superblock(), r.frames()),
        (*new.superblock(), new.frames())
    );
}

// ---------------------------------------------------------------------------------------------------------------
// In place, on disk
// ---------------------------------------------------------------------------------------------------------------

/// The entries of `dir`, sorted, without the writer lock files Windows keeps beside containers (SPEC §4.5.1).
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| !(cfg!(windows) && Path::new(n).extension().is_some_and(|e| e == "lock")))
        .collect();
    names.sort();
    names
}

#[test]
fn compact_replaces_the_file_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let b = build(Container::create(&path).unwrap());
    let mut c = b.c;
    let before = std::fs::read(&path).unwrap();
    // A reader that opened the old file keeps reading it.
    let reader = Container::open(&path, Mode::ReadOnly).unwrap();

    let relocation = c
        .compact(&[range(b.live)], new_manifest(b.live), NONE)
        .unwrap();
    assert_eq!(names(dir.path()), ["app.persia"], "no temporary file left");
    let after = std::fs::read(&path).unwrap();
    assert!(
        after.len() < before.len(),
        "{} -> {}",
        before.len(),
        after.len()
    );
    let new_live = relocation.get(b.live.offset).unwrap();
    assert_eq!(
        payload(&c, c.superblock().manifest_off),
        manifest_for(new_live.offset)
    );
    assert_eq!(
        payload(&reader, b.live.offset),
        vec![2; 4000],
        "the old file is still readable"
    );

    // The handle still holds the writer lock, now on the new file, and keeps working.
    assert!(matches!(
        Container::open(&path, Mode::ReadWrite),
        Err(Error::Locked)
    ));
    let m = c.append(FrameKind::Manifest, b"m4", NONE).unwrap();
    let wal_start = c.superblock().wal_start;
    c.commit(Commit {
        manifest: m,
        wal_start,
        fence_token: 9,
    })
    .unwrap();
    drop(c);
    let r = Container::open(&path, Mode::ReadWrite).unwrap();
    assert_eq!(payload(&r, r.superblock().manifest_off), b"m4");
    assert_eq!(payload(&r, new_live.offset), vec![2; 4000]);
}

#[test]
fn failed_compact_leaves_the_old_file_and_no_temporary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let b = build(Container::create(&path).unwrap());
    let mut c = b.c;
    let before = std::fs::read(&path).unwrap();
    let err = c
        .compact(&[b.live.offset..b.live.end() - 8], |_| Vec::new(), NONE)
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(names(dir.path()), ["app.persia"]);
    c.append(FrameKind::WalChunk, b"still writable", NONE)
        .unwrap();
    c.sync().unwrap();
}

#[test]
fn a_stale_compaction_file_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(Container::create(&path).unwrap());
    std::fs::write(
        dir.path().join("app.persia.compact.0000000000000000.0"),
        b"half written",
    )
    .unwrap();
    Container::open(&path, Mode::ReadWrite).unwrap();
}

#[test]
fn compact_needs_a_path() {
    // A container opened on a bare file (not by path) has nothing to rename over.
    let file = SimFile::new();
    let mut c = Container::create_on(file.clone(), [5; 16]).unwrap();
    let before = file.bytes();
    let err = c.compact(&[], |_| Vec::new(), NONE).unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    assert_eq!(file.bytes(), before);
}

/// After a compaction, the path names the new file; a writer opening it later gets the new file, never the old
/// inode the compacting handle released (the identity check in `Container::open` covers the race in between).
#[test]
fn opening_after_compaction_gets_the_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let b = build(Container::create(&path).unwrap());
    let mut c = b.c;
    let early = std::fs::File::open(&path).unwrap(); // a process that opened the path before the rename
    c.compact(&[range(b.live)], new_manifest(b.live), NONE)
        .unwrap();
    let seq = c.superblock().seq;
    drop(c);
    // The old inode's lock is free again, but the path no longer names it.
    early.try_lock().unwrap();
    let r = Container::open(&path, Mode::ReadWrite).unwrap();
    assert_eq!(r.superblock().seq, seq);
}

#[cfg(unix)]
#[test]
fn compacting_through_a_symlink_replaces_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.persia");
    let link = dir.path().join("link.persia");
    let b = build(Container::create(&real).unwrap());
    drop(b.c);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut c = Container::open(&link, Mode::ReadWrite).unwrap();
    c.compact(&[range(b.live)], new_manifest(b.live), NONE)
        .unwrap();
    let seq = c.superblock().seq;
    drop(c);
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    for name in [&real, &link] {
        assert_eq!(
            Container::open(name, Mode::ReadOnly)
                .unwrap()
                .superblock()
                .seq,
            seq
        );
    }
    assert_eq!(names(dir.path()), ["link.persia", "real.persia"]);
}

// Unix only: Windows exposes no file identity, so the check is skipped there (ROADMAP 2.4.18).
#[cfg(unix)]
#[test]
fn compact_refuses_a_path_replaced_from_outside() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let b = build(Container::create(&path).unwrap());
    let mut c = b.c;
    let intruder = dir.path().join("other");
    std::fs::write(&intruder, b"someone else's file").unwrap();
    std::fs::rename(&intruder, &path).unwrap();
    let err = c
        .compact(&[range(b.live)], new_manifest(b.live), NONE)
        .unwrap_err();
    if cfg!(unix) {
        assert!(matches!(err, Error::InvalidInput(_)), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"someone else's file");
        assert_eq!(names(dir.path()), ["app.persia"]);
    }
}

#[test]
fn compacting_twice_keeps_numbering_and_offsets_stable() {
    let mut b = sim();
    let (mut first, r1) =
        b.c.compact_into(SimFile::new(), &[range(b.live)], new_manifest(b.live), NONE)
            .unwrap();
    let live1 = r1.get(b.live.offset).unwrap();
    let next1 = first.next_frame_seq();
    let (second, r2) = first
        .compact_into(SimFile::new(), &[range(live1)], new_manifest(live1), NONE)
        .unwrap();
    let live2 = r2.get(live1.offset).unwrap();
    assert_eq!(live2.offset, live1.offset, "the live frame stays first");
    assert_eq!(live2.frame_seq, next1);
    assert!(second.next_frame_seq() > next1);
    // The first compaction's Manifest lies in the WAL range, so it is kept; the file grows by the new Manifest.
    let new_manifest_len = second.superblock().manifest_len as usize;
    assert_eq!(
        second.file().bytes().len(),
        first.file().bytes().len() + new_manifest_len
    );
    let r =
        Container::open_on(SimFile::from_bytes(second.file().bytes()), Mode::ReadWrite).unwrap();
    assert_eq!(payload(&r, live2.offset), vec![2; 4000]);
}

#[test]
fn a_failed_pre_sync_poisons_and_writes_no_target() {
    let mut b = sim();
    b.c.append(FrameKind::WalChunk, b"unsynced", NONE).unwrap();
    let file = b.c.file().clone();
    file.set_fault(Some(SimFault::FailAt(file.mutations())));
    let target = SimFile::new();
    let err =
        b.c.compact_into(target.clone(), &[], |_| Vec::new(), NONE)
            .unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
    assert_eq!(target.mutations(), 0);
    assert!(matches!(b.c.sync(), Err(Error::Poisoned)));
}

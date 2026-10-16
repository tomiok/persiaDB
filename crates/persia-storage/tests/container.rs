//! The single-file container (SPEC §4, ROADMAP 2.4.1-2.4.4): creating, locking, read-only opens, appends,
//! commits, and recovery of torn tails, corruption and stale frames.

// Test-only code: unwraps, indexing and casts are the point.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

use std::path::{Path, PathBuf};

use persia_format::block::Compression;
use persia_format::checksum::{Crc32c, crc32c};
use persia_format::frame::{FrameKind, ScanEnd, encode_frame};
use persia_format::superblock::{FIRST_FRAME_OFFSET, SLOT_LEN, Slot, Superblock};
use persia_format::{Corruption, SuperblockField, Writer};
use persia_storage::container::sim::{SimFault, SimFile};
use persia_storage::container::{
    Commit, Container, ContainerFile, Error, FrameRef, Mode, ReadOnlyReason,
};

const MAX: usize = 1 << 20;
const NONE: Compression = Compression::None;
const F: u64 = FIRST_FRAME_OFFSET;

fn new_sim() -> Container<SimFile> {
    Container::create_on(SimFile::new(), [7; 16]).unwrap()
}

fn reopen(file: &SimFile, mode: Mode) -> Result<Container<SimFile>, Error> {
    Container::open_on(SimFile::from_bytes(file.bytes()), mode)
}

/// Appends WAL frames and a Manifest, syncing before the Manifest (SPEC §4.5), and commits them.
fn commit_batch(c: &mut Container<impl ContainerFile>, wal: &[&[u8]], manifest: &[u8]) -> FrameRef {
    for payload in wal {
        c.append(FrameKind::WalChunk, payload, NONE).unwrap();
    }
    c.sync().unwrap();
    let m = c.append(FrameKind::Manifest, manifest, NONE).unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: c.superblock().wal_start,
        fence_token: 0,
    })
    .unwrap();
    m
}

fn manifest(c: &Container<impl ContainerFile>) -> Vec<u8> {
    c.read_frame(c.superblock().manifest_off, MAX)
        .unwrap()
        .payload
}

fn corruption(err: &Error) -> Option<Corruption> {
    match err {
        Error::Format(e) => e.corruption().map(|(_, r)| r),
        Error::CorruptTail { source, .. } => source.corruption().map(|(_, r)| r),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Create, open, lock (ROADMAP 2.4.1)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_new_container_is_the_golden_prefix() {
    let c = new_sim();
    let golden = std::fs::read(fixture("empty.persia")).unwrap();
    let mut bytes = c.file().bytes();
    assert_eq!(bytes.len(), golden.len());
    // Only the uuid (and so the header CRC) differs from the fixture's.
    bytes[32..48].copy_from_slice(&golden[32..48]);
    bytes[12..16].copy_from_slice(&golden[12..16]);
    assert_eq!(bytes, golden);
    assert_eq!(
        c.file().durable_bytes(),
        c.file().bytes(),
        "created durably"
    );
    assert_eq!(
        (c.superblock().seq, c.active_slot(), c.end()),
        (0, Slot::A, F)
    );
    assert_eq!(c.next_frame_seq(), 1);
}

#[test]
fn create_on_needs_an_empty_file() {
    let err = Container::create_on(SimFile::from_bytes(vec![0; 1]), [0; 16]).unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
}

#[test]
fn create_open_round_trip_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut c = Container::create(&path).unwrap();
    let uuid = c.header().db_uuid;
    commit_batch(&mut c, &[b"put 1", b"put 2"], b"manifest 1");
    drop(c);
    assert_eq!(names(dir.path()), ["app.persia"], "no temporary file left");

    let mut c = Container::open(&path, Mode::ReadWrite).unwrap();
    assert_eq!(c.header().db_uuid, uuid);
    assert_eq!((c.superblock().seq, c.active_slot()), (1, Slot::B));
    assert_eq!(manifest(&c), b"manifest 1");
    commit_batch(&mut c, &[b"put 3"], b"manifest 2");
    drop(c);
    let c = Container::open(&path, Mode::ReadOnly).unwrap();
    assert_eq!((c.superblock().seq, c.active_slot()), (2, Slot::A));
    assert_eq!(manifest(&c), b"manifest 2");
    assert_eq!(c.next_frame_seq(), 6);
}

#[test]
fn db_uuids_differ() {
    let dir = tempfile::tempdir().unwrap();
    let a = Container::create(dir.path().join("a.persia")).unwrap();
    let b = Container::create(dir.path().join("b.persia")).unwrap();
    assert_ne!(a.header().db_uuid, b.header().db_uuid);
}

/// SPEC §4.7, ROADMAP 2.4.4.2: creating a container never truncates or replaces an existing file.
#[test]
fn create_never_replaces_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    std::fs::write(&path, b"precious").unwrap();
    let err = Container::create(&path).unwrap_err();
    assert!(
        matches!(&err, Error::AlreadyExists { path: p } if *p == path),
        "{err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"precious");

    // Nor an existing container, which stays openable with its data.
    let other = dir.path().join("db.persia");
    let mut c = Container::create(&other).unwrap();
    commit_batch(&mut c, &[b"x"], b"m");
    drop(c);
    let before = std::fs::read(&other).unwrap();
    assert!(matches!(
        Container::create(&other),
        Err(Error::AlreadyExists { .. })
    ));
    assert_eq!(std::fs::read(&other).unwrap(), before);
    assert_eq!(
        names(dir.path()),
        ["app.persia", "db.persia"],
        "no temporary file left"
    );
}

#[test]
fn create_in_a_missing_directory_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let err = Container::create(dir.path().join("missing/app.persia")).unwrap_err();
    assert!(matches!(err, Error::Io { op: "create", .. }), "{err}");
}

/// ROADMAP 2.4.1.1: one writer at a time, in this process or another; readers take no lock.
#[test]
fn writer_lock_is_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = Container::create(&path).unwrap();
    let err = Container::open(&path, Mode::ReadWrite).unwrap_err();
    assert!(matches!(err, Error::Locked), "{err}");
    assert_eq!(err.to_string(), "container is locked by another writer");
    let reader = Container::open(&path, Mode::ReadOnly).unwrap();
    drop(writer);
    let writer = Container::open(&path, Mode::ReadWrite).unwrap();
    assert!(matches!(
        Container::open(&path, Mode::ReadWrite),
        Err(Error::Locked)
    ));
    drop((reader, writer));
}

#[test]
fn opening_a_missing_or_short_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let err = Container::open(dir.path().join("none.persia"), Mode::ReadOnly).unwrap_err();
    assert!(matches!(err, Error::Io { op: "open", .. }), "{err}");
    for len in [0, 100, 4096, 8192, F as usize - 1] {
        let bytes = new_sim().file().bytes()[..len].to_vec();
        let err = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadOnly).unwrap_err();
        assert!(err.is_corruption(), "{len}: {err}");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Read-only (ROADMAP 2.4.1.2)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn read_only_handles_refuse_writes() {
    let mut c = new_sim();
    commit_batch(&mut c, &[b"a"], b"m");
    let mut ro = reopen(c.file(), Mode::ReadOnly).unwrap();
    assert_eq!(ro.mode(), Mode::ReadOnly);
    let read_only = |e: Error| {
        matches!(
            e,
            Error::ReadOnly {
                reason: ReadOnlyReason::OpenedReadOnly
            }
        )
    };
    assert!(read_only(
        ro.append(FrameKind::WalChunk, b"x", NONE).unwrap_err()
    ));
    assert!(read_only(ro.sync().unwrap_err()));
    let m = ro.frames().last().copied().unwrap();
    let commit = Commit {
        manifest: m,
        wal_start: ro.superblock().wal_start,
        fence_token: 0,
    };
    assert!(read_only(ro.commit(commit).unwrap_err()));
}

/// SPEC §4.2 step 6: unknown `rocompat_flags` allow reading, not writing.
#[test]
fn unknown_rocompat_flags_open_only_read_only() {
    let mut c = new_sim();
    commit_batch(&mut c, &[b"a"], b"m");
    let mut bytes = c.file().bytes();
    bytes[24..32].copy_from_slice(&0x8000_0000_0000_0000_u64.to_le_bytes());
    let mut crc = Crc32c::new();
    crc.update(&bytes[..12]);
    crc.update(&bytes[16..4096]);
    bytes[12..16].copy_from_slice(&crc.finish().to_le_bytes());

    let err = Container::open_on(SimFile::from_bytes(bytes.clone()), Mode::ReadWrite).unwrap_err();
    assert!(
        matches!(
            err,
            Error::ReadOnly {
                reason: ReadOnlyReason::UnknownRocompatFlags {
                    flags: 0x8000_0000_0000_0000
                }
            }
        ),
        "{err}"
    );
    let ro = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadOnly).unwrap();
    assert_eq!(manifest(&ro), b"m");
}

#[test]
fn unknown_incompat_flags_do_not_open() {
    let mut bytes = new_sim().file().bytes();
    bytes[16] = 0x40;
    let mut crc = Crc32c::new();
    crc.update(&bytes[..12]);
    crc.update(&bytes[16..4096]);
    bytes[12..16].copy_from_slice(&crc.finish().to_le_bytes());
    for mode in [Mode::ReadOnly, Mode::ReadWrite] {
        let err = Container::open_on(SimFile::from_bytes(bytes.clone()), mode).unwrap_err();
        assert!(
            matches!(err, Error::Format(persia_format::Error::Unsupported { .. })),
            "{err}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Append, sync, commit (ROADMAP 2.4.2, 2.4.3)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn appends_are_numbered_and_visible_before_sync() {
    let mut c = new_sim();
    let a = c.append(FrameKind::WalChunk, b"one", NONE).unwrap();
    let b = c
        .append(
            FrameKind::Segment,
            &[9; 4000],
            Compression::Zstd { level: 3 },
        )
        .unwrap();
    assert_eq!(
        (a.offset, a.frame_seq, b.offset, b.frame_seq),
        (F, 1, a.end(), 2)
    );
    assert_eq!(c.end(), b.end());
    assert!(c.has_unsynced());
    assert_eq!(
        c.file().durable_bytes().len() as u64,
        F,
        "nothing synced yet"
    );
    assert_eq!(c.read_frame(b.offset, MAX).unwrap().payload, vec![9; 4000]);
    c.sync().unwrap();
    assert!(!c.has_unsynced());
    assert_eq!(c.file().durable_bytes().len() as u64, c.end());
    let syncs = c.file().mutations();
    c.sync().unwrap();
    assert_eq!(c.file().mutations(), syncs, "an idle sync does no I/O");
    assert_eq!(c.frames(), [a, b]);
}

#[test]
fn bad_compression_level_appends_nothing() {
    let mut c = new_sim();
    let err = c
        .append(FrameKind::WalChunk, b"x", Compression::Zstd { level: 1000 })
        .unwrap_err();
    assert!(matches!(err, Error::Encode(_)), "{err}");
    assert_eq!((c.end(), c.next_frame_seq()), (F, 1));
    assert!(c.append(FrameKind::WalChunk, b"x", NONE).is_ok());
}

#[test]
fn commit_writes_the_inactive_slot_and_alternates() {
    let mut c = new_sim();
    for i in 1..=5_u64 {
        let m = commit_batch(&mut c, &[b"put"], format!("manifest {i}").as_bytes());
        let sb = *c.superblock();
        let slot = if i % 2 == 1 { Slot::B } else { Slot::A };
        assert_eq!(c.active_slot(), slot);
        assert_eq!(
            sb,
            Superblock {
                seq: i,
                manifest_off: m.offset,
                manifest_len: m.len,
                log_end: c.end(),
                wal_start: F,
                fence_token: 0,
                next_frame_seq: 2 * i + 1,
            }
        );
        assert_eq!(
            c.file().durable_bytes(),
            c.file().bytes(),
            "a commit is durable"
        );
        // The other slot still holds the previous commit.
        let other = &c.file().bytes()[slot.other().offset() as usize..][..SLOT_LEN];
        let prev = Superblock::decode(&mut persia_format::Reader::new(other)).unwrap();
        assert_eq!(prev.seq, i - 1);
        let r = reopen(c.file(), Mode::ReadWrite).unwrap();
        assert_eq!((*r.superblock(), r.active_slot()), (sb, slot));
        assert_eq!(manifest(&r), format!("manifest {i}").as_bytes());
    }
}

#[test]
fn wal_start_advances_and_drops_older_frames() {
    let mut c = new_sim();
    let w1 = c.append(FrameKind::WalChunk, b"w1", NONE).unwrap();
    let w2 = c.append(FrameKind::WalChunk, b"w2", NONE).unwrap();
    c.sync().unwrap();
    let m = c.append(FrameKind::Manifest, b"m", NONE).unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: w2.offset,
        fence_token: 5,
    })
    .unwrap();
    assert_eq!(c.frames(), [w2, m]);
    assert_eq!(c.superblock().fence_token, 5);
    let r = reopen(c.file(), Mode::ReadOnly).unwrap();
    assert_eq!(r.frames(), [w2, m]);
    assert!(r.frames().iter().all(|f| f.offset != w1.offset));
    // WAL fully consumed: wal_start = log_end, the Manifest below it.
    let m2 = c.append(FrameKind::Manifest, b"m2", NONE).unwrap();
    c.commit(Commit {
        manifest: m2,
        wal_start: c.end(),
        fence_token: 5,
    })
    .unwrap();
    assert_eq!(c.frames(), []);
    let r = reopen(c.file(), Mode::ReadWrite).unwrap();
    assert_eq!(manifest(&r), b"m2");
    assert_eq!(r.next_frame_seq(), 5);
}

#[test]
fn commit_rejects_bad_arguments() {
    let mut c = new_sim();
    let w = c.append(FrameKind::WalChunk, b"w", NONE).unwrap();
    let m = c.append(FrameKind::Manifest, b"m", NONE).unwrap();
    let ok = Commit {
        manifest: m,
        wal_start: w.offset,
        fence_token: 0,
    };
    let bad = [
        Commit { manifest: w, ..ok }, // not a Manifest
        Commit {
            manifest: FrameRef {
                len: m.len + 8,
                ..m
            },
            ..ok
        }, // not this frame
        Commit {
            manifest: FrameRef { offset: 0, ..m },
            ..ok
        },
        Commit {
            wal_start: w.offset + 8,
            ..ok
        }, // not a frame start
        Commit {
            wal_start: F - 8,
            ..ok
        },
        Commit {
            wal_start: c.end() + 8,
            ..ok
        },
    ];
    for commit in bad {
        let err = c.commit(commit).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{commit:?}: {err}");
    }
    assert_eq!(c.superblock().seq, 0, "nothing committed");
    c.commit(ok).unwrap();
    // Advance wal_start past every frame so far: those frames are gone from the handle.
    let m2 = c.append(FrameKind::Manifest, b"m2", NONE).unwrap();
    let advanced = Commit {
        manifest: m2,
        wal_start: m2.offset,
        fence_token: 0,
    };
    c.commit(advanced).unwrap();
    // wal_start never moves back, and a Manifest dropped by it can no longer be committed.
    for commit in [
        Commit {
            wal_start: w.offset,
            ..advanced
        },
        Commit {
            manifest: m,
            ..advanced
        },
    ] {
        let err = c.commit(commit).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{commit:?}: {err}");
    }
    c.commit(advanced).unwrap();
    assert_eq!(c.superblock().seq, 3);
}

#[test]
fn a_failed_write_poisons_the_handle() {
    for call in 0..4 {
        for fault in [SimFault::FailAt(call), SimFault::ApplyThenFail(call)] {
            let mut c = new_sim();
            let base = c.file().mutations();
            c.file().set_fault(Some(match fault {
                SimFault::FailAt(n) => SimFault::FailAt(base + n),
                SimFault::ApplyThenFail(n) => SimFault::ApplyThenFail(base + n),
            }));
            // append, sync (commit step 1), append Manifest, then commit's sync of it, slot write, slot sync.
            let result = (|| {
                c.append(FrameKind::WalChunk, b"w", NONE)?;
                c.sync()?;
                let m = c.append(FrameKind::Manifest, b"m", NONE)?;
                c.commit(Commit {
                    manifest: m,
                    wal_start: F,
                    fence_token: 0,
                })
            })();
            let err = result.unwrap_err();
            assert!(matches!(err, Error::Io { .. }), "{fault:?}: {err}");
            assert!(matches!(
                c.append(FrameKind::WalChunk, b"x", NONE),
                Err(Error::Poisoned)
            ));
            assert!(matches!(c.sync(), Err(Error::Poisoned)));
            assert_eq!(c.superblock().seq, 0, "{fault:?}: no commit published");
        }
    }
}

#[test]
fn read_frame_checks_its_offset() {
    let mut c = new_sim();
    let w = c.append(FrameKind::WalChunk, b"payload", NONE).unwrap();
    let got = c.read_frame(w.offset, MAX).unwrap();
    assert_eq!((got.frame, got.payload.as_slice()), (w, &b"payload"[..]));
    for offset in [0, F - 8, w.end(), w.end() + 8, u64::MAX] {
        let err = c.read_frame(offset, MAX).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{offset}: {err}");
    }
    let err = c.read_frame(w.offset + 8, MAX).unwrap_err();
    assert!(err.is_corruption(), "{err}");
    let err = c.read_frame(w.offset, 3).unwrap_err();
    assert_eq!(
        corruption(&err),
        Some(Corruption::UncompressedTooLarge { len: 7, max: 3 })
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Recovery (ROADMAP 2.4.4)
// ---------------------------------------------------------------------------------------------------------------

/// A committed container with one uncommitted, synced WAL frame after `log_end`.
fn with_tail() -> (Container<SimFile>, FrameRef) {
    let mut c = new_sim();
    commit_batch(&mut c, &[b"w1", b"w2"], b"m1");
    let tail = c
        .append(FrameKind::WalChunk, b"acknowledged", NONE)
        .unwrap();
    c.sync().unwrap();
    (c, tail)
}

#[test]
fn synced_frames_after_log_end_survive_and_get_committed() {
    let (c, tail) = with_tail();
    let mut r = reopen(c.file(), Mode::ReadWrite).unwrap();
    assert_eq!(r.uncommitted_frames(), [tail]);
    assert_eq!(r.recovery().scan_end, ScanEnd::Clean { offset: tail.end() });
    assert_eq!((r.end(), r.next_frame_seq()), (tail.end(), 5));
    commit_batch(&mut r, &[], b"m2");
    assert!(r.superblock().log_end > tail.end());
    let r = reopen(r.file(), Mode::ReadOnly).unwrap();
    assert_eq!(r.uncommitted_frames(), []);
    assert!(r.frames().contains(&tail));
}

/// ROADMAP 2.4.4.1: a torn tail is truncated (read-write, durably) or ignored (read-only).
#[test]
fn torn_tail_is_truncated_by_writers_only() {
    let (c, tail) = with_tail();
    let mut bytes = c.file().bytes();
    let mut w = Writer::new();
    encode_frame(&mut w, FrameKind::WalChunk, 5, &[0xA5; 300], NONE).unwrap();
    bytes.extend_from_slice(&w.as_slice()[..100]); // an append cut short
    let len = bytes.len() as u64;

    let ro = Container::open_on(SimFile::from_bytes(bytes.clone()), Mode::ReadOnly).unwrap();
    assert!(
        matches!(ro.recovery().scan_end, ScanEnd::TornTail { offset, .. } if offset == tail.end())
    );
    assert_eq!((ro.end(), ro.recovery().truncated_from), (tail.end(), None));
    assert_eq!(ro.file().bytes(), bytes, "a reader never writes");

    let file = SimFile::from_bytes(bytes);
    let mut rw = Container::open_on(file.clone(), Mode::ReadWrite).unwrap();
    assert_eq!(rw.recovery().truncated_from, Some(len));
    assert_eq!(
        file.durable_bytes().len() as u64,
        tail.end(),
        "truncated and synced"
    );
    // The next append lands where the torn frame was, and the result reopens cleanly.
    let next = rw.append(FrameKind::WalChunk, b"after", NONE).unwrap();
    assert_eq!((next.offset, next.frame_seq), (tail.end(), 5));
    rw.sync().unwrap();
    let r = reopen(&file, Mode::ReadOnly).unwrap();
    assert_eq!(r.uncommitted_frames(), [tail, next]);
}

#[test]
fn zero_filled_tail_is_torn() {
    let (c, tail) = with_tail();
    let mut bytes = c.file().bytes();
    bytes.resize(bytes.len() + 4096, 0);
    let rw = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadWrite).unwrap();
    assert!(matches!(rw.recovery().scan_end, ScanEnd::TornTail { .. }));
    assert_eq!(rw.file().bytes().len() as u64, tail.end());
}

/// SPEC §4.4: after corruption the bytes may hold acknowledged writes; a writer refuses to open, a reader opens.
#[test]
fn corrupt_tail_is_never_truncated() {
    let (c, tail) = with_tail();
    let mut bytes = c.file().bytes();
    bytes[tail.offset as usize + 30] ^= 1; // damage the tail frame's payload
    let mut w = Writer::new();
    encode_frame(&mut w, FrameKind::WalChunk, 6, b"later", NONE).unwrap();
    bytes.extend_from_slice(w.as_slice());

    let file = SimFile::from_bytes(bytes.clone());
    let err = Container::open_on(file.clone(), Mode::ReadWrite).unwrap_err();
    assert!(
        matches!(&err, Error::CorruptTail { offset, .. } if *offset == tail.offset),
        "{err}"
    );
    assert!(err.is_corruption());
    assert_eq!(file.bytes(), bytes, "nothing truncated");

    let ro = Container::open_on(file.clone(), Mode::ReadOnly).unwrap();
    assert!(
        matches!(ro.recovery().scan_end, ScanEnd::Corrupt { offset, .. } if offset == tail.offset)
    );
    assert_eq!(ro.end(), tail.offset);
    assert_eq!(manifest(&ro), b"m1");
    assert_eq!(file.bytes(), bytes);
}

/// ADR-0013, ADR-0014: a whole frame after `log_end` with the wrong `frame_seq` ends the scan as corruption. One
/// below the expected value is a leftover and is truncated; one above it (or `u64::MAX`) may follow a lost frame and
/// hold an acknowledged write, so a read-write open refuses.
#[test]
fn stale_frame_after_log_end_is_corruption() {
    for seq in [4, 3, 6, 0, u64::MAX] {
        let mut c = new_sim();
        commit_batch(&mut c, &[b"w1", b"w2"], b"m1"); // frames 1..=3, next 4
        let mut bytes = c.file().bytes();
        let committed_len = bytes.len() as u64;
        let mut w = Writer::new();
        encode_frame(&mut w, FrameKind::WalChunk, seq, b"x", NONE).unwrap();
        bytes.extend_from_slice(w.as_slice());
        let ro = Container::open_on(SimFile::from_bytes(bytes.clone()), Mode::ReadOnly).unwrap();
        let file = SimFile::from_bytes(bytes.clone());
        let rw = Container::open_on(file.clone(), Mode::ReadWrite);
        if seq == 4 {
            assert_eq!(rw.unwrap().uncommitted_frames().len(), 1);
            continue;
        }
        assert!(
            matches!(
                ro.recovery().scan_end,
                ScanEnd::Corrupt {
                    error: persia_format::Error::Corrupt {
                        reason: Corruption::FrameSeqMismatch { expected: 4, .. }
                            | Corruption::FrameSeqOutOfRange { .. },
                        ..
                    },
                    ..
                }
            ),
            "{seq}: {:?}",
            ro.recovery()
        );
        if seq < 4 {
            let c = rw.unwrap();
            assert_eq!(c.end(), committed_len, "{seq}");
            assert_eq!(c.recovery().truncated_from, Some(bytes.len() as u64));
            assert_eq!(c.recovery().quarantined, None, "nothing worth keeping");
            assert_eq!(file.durable_bytes().len() as u64, committed_len);
        } else {
            let err = rw.unwrap_err();
            assert!(
                matches!(err, Error::CorruptTail { acknowledged: Some(at), .. } if at == committed_len),
                "{seq}: {err}"
            );
            assert_eq!(file.bytes(), bytes, "{seq}: nothing changed");
        }
    }
}

/// Changes the bytes of a container.
type Damage = Box<dyn Fn(&mut Vec<u8>)>;

/// ROADMAP 2.4.4.2, SPEC §8.6: an invalid frame in `wal_start..log_end` is corruption and is never truncated.
#[test]
fn invalid_committed_frame_is_corruption_in_both_modes() {
    let mut c = new_sim();
    commit_batch(&mut c, &[b"wal one", b"wal two"], b"m1");
    let w2 = c.frames()[1];
    let damages: [(&str, Damage); 4] = [
        (
            "payload bit",
            Box::new(move |b| b[w2.offset as usize + 26] ^= 4),
        ),
        ("magic", Box::new(move |b| b[w2.offset as usize] = b'X')),
        // Zeros inside the committed range look like a torn tail, but these frames were committed.
        (
            "zeros",
            Box::new(move |b| b[w2.offset as usize..w2.end() as usize].fill(0)),
        ),
        ("frame_seq", Box::new(move |b| renumber(b, w2.offset, 9))),
    ];
    for (what, damage) in damages {
        let mut bytes = c.file().bytes();
        damage(&mut bytes);
        for mode in [Mode::ReadOnly, Mode::ReadWrite] {
            let file = SimFile::from_bytes(bytes.clone());
            let err = Container::open_on(file.clone(), mode).unwrap_err();
            assert!(
                matches!(err, Error::Format(_)) && err.is_corruption(),
                "{what}: {err}"
            );
            assert_eq!(file.bytes(), bytes, "{what}: never truncated");
        }
    }
}

/// The committed run must end at `next_frame_seq - 1` (ADR-0013).
#[test]
fn committed_run_must_end_at_next_frame_seq() {
    let mut c = new_sim();
    commit_batch(&mut c, &[b"a"], b"m");
    let mut bytes = c.file().bytes();
    let slot = c.active_slot();
    rewrite_slot(&mut bytes, slot, |sb| sb.next_frame_seq = 9);
    let err = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadOnly).unwrap_err();
    assert_eq!(
        corruption(&err),
        Some(Corruption::FrameSeqMismatch {
            expected: 8,
            found: 2
        })
    );
}

#[test]
fn superblock_must_fit_the_file() {
    let mut c = new_sim();
    commit_batch(&mut c, &[b"a"], b"m");
    // The file lost committed bytes (e.g. truncated by another program).
    let mut bytes = c.file().bytes();
    bytes.truncate(bytes.len() - 8);
    let err = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadOnly).unwrap_err();
    assert!(
        matches!(
            corruption(&err),
            Some(Corruption::InvalidSuperblock {
                field: SuperblockField::LogEnd,
                ..
            })
        ),
        "{err}"
    );
}

#[test]
fn superblock_must_point_at_a_manifest() {
    let mut c = new_sim();
    commit_batch(&mut c, &[b"w"], b"m");
    let w = c.frames()[0];
    let slot = c.active_slot();
    let sb = *c.superblock();
    for (off, len) in [(w.offset, w.len), (sb.manifest_off, sb.manifest_len - 8)] {
        let mut bytes = c.file().bytes();
        rewrite_slot(&mut bytes, slot, |sb| {
            sb.manifest_off = off;
            sb.manifest_len = len;
        });
        let err = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadOnly).unwrap_err();
        assert!(
            matches!(
                corruption(&err),
                Some(Corruption::NotAManifest { .. } | Corruption::UnexpectedEof { .. })
            ),
            "{err}"
        );
    }
}

/// A torn write of the new slot leaves the previous commit active; its frames are found after `log_end`.
#[test]
fn torn_slot_keeps_the_previous_commit() {
    let mut c = new_sim();
    commit_batch(&mut c, &[b"w1"], b"m1");
    let m1 = c.superblock().seq;
    commit_batch(&mut c, &[b"w2"], b"m2");
    let mut bytes = c.file().bytes();
    let slot = c.active_slot();
    let at = slot.offset() as usize;
    bytes[at + 2048..at + SLOT_LEN].fill(0);
    let r = Container::open_on(SimFile::from_bytes(bytes), Mode::ReadWrite).unwrap();
    assert_eq!((r.superblock().seq, r.active_slot()), (m1, slot.other()));
    assert_eq!(manifest(&r), b"m1");
    assert_eq!(r.uncommitted_frames().len(), 2);
}

// ---------------------------------------------------------------------------------------------------------------
// Golden fixtures (SPEC §15)
// ---------------------------------------------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/format/v1.0")
        .join(name)
}

#[test]
fn golden_fixtures_open() {
    struct Case {
        name: &'static str,
        seq: u64,
        uncommitted: usize,
        /// `None`: the read-write open fails with `CorruptTail`.
        truncated: Option<bool>,
    }
    let cases = [
        Case {
            name: "empty.persia",
            seq: 0,
            uncommitted: 0,
            truncated: Some(false),
        },
        Case {
            name: "frames.persia",
            seq: 1,
            uncommitted: 0,
            truncated: Some(false),
        },
        Case {
            name: "torn-tail.persia",
            seq: 1,
            uncommitted: 0,
            truncated: Some(true),
        },
        Case {
            name: "torn-zeros.persia",
            seq: 1,
            uncommitted: 0,
            truncated: Some(true),
        },
        Case {
            name: "corrupt-frame.persia",
            seq: 1,
            uncommitted: 0,
            truncated: None,
        },
        Case {
            name: "torn-slot.persia",
            seq: 1,
            uncommitted: 2,
            truncated: Some(false),
        },
    ];
    for case in cases {
        let bytes = std::fs::read(fixture(case.name)).unwrap();
        let ro = Container::open_on(SimFile::from_bytes(bytes.clone()), Mode::ReadOnly).unwrap();
        assert_eq!(ro.superblock().seq, case.seq, "{}", case.name);
        assert_eq!(
            ro.uncommitted_frames().len(),
            case.uncommitted,
            "{}",
            case.name
        );
        if case.seq > 0 {
            assert_eq!(manifest(&ro), b"manifest: segment frame 5", "{}", case.name);
        }
        let rw = Container::open_on(SimFile::from_bytes(bytes.clone()), Mode::ReadWrite);
        match case.truncated {
            None => assert!(
                matches!(rw, Err(Error::CorruptTail { .. })),
                "{}",
                case.name
            ),
            Some(truncated) => {
                let rw = rw.unwrap();
                assert_eq!(
                    rw.recovery().truncated_from.is_some(),
                    truncated,
                    "{}",
                    case.name
                );
                assert_eq!(rw.end(), ro.end(), "{}", case.name);
                assert_eq!(rw.file().bytes().len() as u64, rw.end(), "{}", case.name);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Helpers that damage containers
// ---------------------------------------------------------------------------------------------------------------

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Sets the `frame_seq` of the frame at `offset` and fixes its CRC.
fn renumber(bytes: &mut [u8], offset: u64, seq: u64) {
    let at = offset as usize;
    bytes[at + 16..at + 24].copy_from_slice(&seq.to_le_bytes());
    let payload_len = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()) as usize;
    let crc_at = at + 24 + payload_len.next_multiple_of(8);
    let crc = crc32c(&bytes[at + 4..crc_at]);
    bytes[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
}

/// Rewrites the superblock in `slot` through `edit`, with a valid CRC.
fn rewrite_slot(bytes: &mut [u8], slot: Slot, edit: impl FnOnce(&mut Superblock)) {
    let at = slot.offset() as usize;
    let mut sb =
        Superblock::decode(&mut persia_format::Reader::new(&bytes[at..at + SLOT_LEN])).unwrap();
    edit(&mut sb);
    let mut w = Writer::new();
    sb.encode(&mut w).unwrap();
    bytes[at..at + SLOT_LEN].copy_from_slice(w.as_slice());
}

/// A file that records the largest read, to check that recovery reads in bounded chunks (ROADMAP 2.4.12).
#[derive(Debug, Clone)]
struct ReadMeter {
    file: SimFile,
    largest: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl ContainerFile for ReadMeter {
    fn file_len(&self) -> std::io::Result<u64> {
        self.file.file_len()
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.largest
            .fetch_max(buf.len(), std::sync::atomic::Ordering::Relaxed);
        self.file.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        self.file.write_all_at(buf, offset)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.file.sync_data()
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)
    }
    fn try_lock(&self) -> std::io::Result<bool> {
        self.file.try_lock()
    }
}

#[test]
fn recovery_memory_does_not_grow_with_frame_size() {
    const BIG: usize = 8 << 20;
    let mut c = new_sim();
    let wal = c
        .append(FrameKind::WalChunk, &vec![1; BIG], Compression::None)
        .unwrap();
    c.sync().unwrap();
    let m = c
        .append(FrameKind::Manifest, b"m", Compression::None)
        .unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: wal.offset,
        fence_token: 0,
    })
    .unwrap();
    // An uncommitted, synced Segment frame, then a large torn tail of zeros.
    c.append(FrameKind::Segment, &vec![2; BIG], Compression::None)
        .unwrap();
    c.sync().unwrap();
    let file = c.file().clone();
    drop(c);
    let len = file.file_len().unwrap();
    file.set_len(len + BIG as u64).unwrap();
    for mode in [Mode::ReadOnly, Mode::ReadWrite] {
        let meter = ReadMeter {
            file: SimFile::from_bytes(file.bytes()),
            largest: std::sync::Arc::default(),
        };
        let c = Container::open_on(meter.clone(), mode).unwrap();
        assert_eq!(c.frames().len(), 3, "{mode:?}");
        assert!(matches!(c.recovery().scan_end, ScanEnd::TornTail { offset, .. } if offset == len));
        let largest = meter.largest.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            largest <= persia_format::frame::DEFAULT_SCAN_CHUNK.max(FIRST_FRAME_OFFSET as usize),
            "{mode:?}: read {largest} bytes at once"
        );
    }
}

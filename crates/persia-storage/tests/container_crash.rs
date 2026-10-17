//! Crash simulation of the container commit protocol (ROADMAP 2.4.3.1; SPEC §4.4, §4.5, §8.6).
//!
//! A commit runs on a [`SimFile`] that fails at one of its writes or syncs (before or after the call takes effect)
//! and the process dies there. The file a restart finds keeps none, all, or a seeded sector-level subset of the
//! unsynced writes. Recovery must then yield the previous commit or the new one, never a mix; an acknowledged commit
//! or sync must survive; and a container recovered read-write must accept the next commit.

// Test-only code: unwraps and indexing are the point.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Commit, Container, Error, FrameRef, Mode};

const NONE: Compression = Compression::None;
const MAX: usize = 1 << 20;

#[cfg(not(feature = "slow"))]
const SEEDS: u64 = 16;
#[cfg(feature = "slow")]
const SEEDS: u64 = 512;

/// What the interrupted run got acknowledged, and where its frames went.
#[derive(Debug, Default)]
struct Run {
    segment: Option<FrameRef>,
    wal: Option<FrameRef>,
    /// Step 1's sync of the Segment and WAL frames returned.
    step1_synced: bool,
    committed: bool,
    /// The WAL frame appended and synced after the commit.
    acked_tail: Option<FrameRef>,
}

fn segment_payload() -> Vec<u8> {
    (0..3000_u32).map(|i| (i % 251) as u8).collect()
}

/// Commit 1: one WAL frame and its Manifest. Returns the container, ready for commit 2.
fn committed_once() -> Container<SimFile> {
    let mut c = Container::create_on(SimFile::new(), [1; 16]).unwrap();
    let w = c.append(FrameKind::WalChunk, b"w1", NONE).unwrap();
    c.sync().unwrap();
    let m = c.append(FrameKind::Manifest, b"m1", NONE).unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: w.offset,
        fence_token: 0,
    })
    .unwrap();
    c
}

/// Commit 2 as SPEC §4.5 orders it, then one acknowledged WAL append; stops at the first error.
fn commit_two(c: &mut Container<SimFile>, run: &mut Run) -> Result<(), Error> {
    run.segment = Some(c.append(FrameKind::Segment, &segment_payload(), NONE)?);
    let wal = c.append(FrameKind::WalChunk, b"w2", NONE)?;
    run.wal = Some(wal);
    c.sync()?; // step 1
    run.step1_synced = true;
    let m = c.append(FrameKind::Manifest, b"m2", NONE)?;
    c.commit(Commit {
        manifest: m,
        wal_start: wal.offset,
        fence_token: 0,
    })?; // steps 2 and 3
    run.committed = true;
    let tail = c.append(FrameKind::WalChunk, b"w3", NONE)?;
    c.sync()?;
    run.acked_tail = Some(tail);
    Ok(())
}

fn payload(c: &Container<SimFile>, offset: u64) -> Vec<u8> {
    c.read_frame(offset, MAX).unwrap().payload
}

/// Checks the recovered container against the run; returns whether it holds commit 2.
fn check_recovered(c: &Container<SimFile>, run: &Run, ctx: &str) -> bool {
    let sb = *c.superblock();
    assert!(sb.seq == 1 || sb.seq == 2, "{ctx}: seq {}", sb.seq);
    assert!(
        sb.seq == 2 || !run.committed,
        "{ctx}: an acknowledged commit was lost"
    );
    let manifest = payload(c, sb.manifest_off);
    if sb.seq == 1 {
        assert_eq!(manifest, b"m1", "{ctx}");
        // The old commit, whole: its WAL range is exactly the first frame and the Manifest.
        let committed: Vec<FrameKind> = c
            .frames()
            .iter()
            .filter(|f| f.offset < sb.log_end)
            .map(|f| f.kind)
            .collect();
        assert_eq!(
            committed,
            [FrameKind::WalChunk, FrameKind::Manifest],
            "{ctx}"
        );
        assert_eq!(payload(c, sb.wal_start), b"w1", "{ctx}");
        if run.step1_synced {
            let uncommitted = c.uncommitted_frames();
            for f in [run.segment.unwrap(), run.wal.unwrap()] {
                assert!(uncommitted.contains(&f), "{ctx}: step 1 was acknowledged");
            }
        }
    } else {
        assert_eq!(manifest, b"m2", "{ctx}");
        // The new commit, whole: the Segment it names and its WAL are readable.
        let (segment, wal) = (run.segment.unwrap(), run.wal.unwrap());
        assert_eq!(payload(c, segment.offset), segment_payload(), "{ctx}");
        assert_eq!(sb.wal_start, wal.offset, "{ctx}");
        assert_eq!(payload(c, wal.offset), b"w2", "{ctx}");
    }
    if let Some(tail) = run.acked_tail {
        assert!(
            c.uncommitted_frames().contains(&tail),
            "{ctx}: an acknowledged sync was lost"
        );
        assert_eq!(payload(c, tail.offset), b"w3", "{ctx}");
    }
    // Every recovered frame is readable, numbered in one run up to the next value.
    for (i, f) in c.frames().iter().enumerate() {
        c.read_frame(f.offset, MAX).unwrap();
        assert_eq!(
            f.frame_seq + (c.frames().len() - i) as u64,
            c.next_frame_seq(),
            "{ctx}"
        );
    }
    sb.seq == 2
}

/// The recovered container accepts another commit, which a reopen then finds.
fn check_writable(mut c: Container<SimFile>, ctx: &str) {
    let seq = c.superblock().seq;
    c.sync().unwrap();
    let m = c.append(FrameKind::Manifest, b"m3", NONE).unwrap();
    let wal_start = c.superblock().wal_start;
    c.commit(Commit {
        manifest: m,
        wal_start,
        fence_token: 0,
    })
    .unwrap();
    let r = Container::open_on(SimFile::from_bytes(c.file().bytes()), Mode::ReadWrite).unwrap();
    assert_eq!(r.superblock().seq, seq + 1, "{ctx}");
    assert_eq!(payload(&r, r.superblock().manifest_off), b"m3", "{ctx}");
}

#[test]
fn crash_at_every_write_of_a_commit_recovers_old_or_new() {
    // How many mutating calls commit 2 makes when nothing fails.
    let base = committed_once().file().mutations();
    let calls = {
        let mut c = committed_once();
        commit_two(&mut c, &mut Run::default()).unwrap();
        c.file().mutations() - base
    };
    assert_eq!(
        calls, 9,
        "append ×2, sync, append, sync, slot write, slot sync, append, sync"
    );

    let mut outcomes = [0_u32; 3]; // old, new, corrupt tail (read-only only)
    for n in base..base + calls {
        for fault in [SimFault::FailAt(n), SimFault::ApplyThenFail(n)] {
            let losses = [CrashLoss::All, CrashLoss::Nothing]
                .into_iter()
                .chain((0..SEEDS).map(CrashLoss::Seeded));
            for loss in losses {
                let ctx = format!("{fault:?} {loss:?}");
                let mut c = committed_once();
                c.file().set_fault(Some(fault));
                let mut run = Run::default();
                assert!(
                    commit_two(&mut c, &mut run).is_err(),
                    "{ctx}: the fault fired"
                );
                let image = c.file().crash(loss);
                let before = image.bytes();
                match Container::open_on(image.clone(), Mode::ReadWrite) {
                    Ok(rw) => {
                        let new = check_recovered(&rw, &run, &ctx);
                        outcomes[usize::from(new)] += 1;
                        check_writable(rw, &ctx);
                    }
                    // Unsynced appends that reached the disk out of order look like corruption (SPEC §4.4); only a
                    // crash that loses part of the unsynced writes can produce that, and the bytes stay intact.
                    Err(Error::CorruptTail { .. }) => {
                        assert!(matches!(loss, CrashLoss::Seeded(_)), "{ctx}");
                        assert_eq!(image.bytes(), before, "{ctx}: nothing truncated");
                        let ro = Container::open_on(image, Mode::ReadOnly).unwrap();
                        check_recovered(&ro, &run, &ctx);
                        outcomes[2] += 1;
                    }
                    Err(err) => panic!("{ctx}: recovery failed: {err}"),
                }
            }
        }
    }
    // The scenario reaches both outcomes; the exact split depends on the seeds.
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}

/// Every crash loss the tests try after a fault.
fn losses() -> impl Iterator<Item = CrashLoss> {
    [CrashLoss::All, CrashLoss::Nothing]
        .into_iter()
        .chain((0..SEEDS).map(CrashLoss::Seeded))
}

/// The first commit (seq 0 to 1) falls back to the new-container state of SPEC §4.3.
#[test]
fn crash_during_the_first_commit_recovers_empty_or_committed() {
    let fresh = || Container::create_on(SimFile::new(), [2; 16]).unwrap();
    let base = fresh().file().mutations();
    for n in base..base + 6 {
        for fault in [SimFault::FailAt(n), SimFault::ApplyThenFail(n)] {
            for loss in losses() {
                let ctx = format!("{fault:?} {loss:?}");
                let mut c = fresh();
                c.file().set_fault(Some(fault));
                let mut committed = false;
                let result = (|| {
                    let w = c.append(FrameKind::WalChunk, b"w1", NONE)?;
                    c.sync()?;
                    let m = c.append(FrameKind::Manifest, b"m1", NONE)?;
                    c.commit(Commit {
                        manifest: m,
                        wal_start: w.offset,
                        fence_token: 0,
                    })?;
                    committed = true;
                    Ok::<(), Error>(())
                })();
                assert!(result.is_err(), "{ctx}: the fault fired");
                let image = c.file().crash(loss);
                let c = match Container::open_on(image.clone(), Mode::ReadWrite) {
                    Ok(c) => c,
                    Err(Error::CorruptTail { .. }) => {
                        Container::open_on(image, Mode::ReadOnly).unwrap()
                    }
                    Err(err) => panic!("{ctx}: {err}"),
                };
                let sb = *c.superblock();
                match sb.seq {
                    0 => {
                        assert!(!committed, "{ctx}: an acknowledged commit was lost");
                        assert_eq!((sb.log_end, sb.next_frame_seq), (12288, 1), "{ctx}");
                    }
                    1 => assert_eq!(payload(&c, sb.manifest_off), b"m1", "{ctx}"),
                    seq => panic!("{ctx}: seq {seq}"),
                }
            }
        }
    }
}

/// Recovery is idempotent (SPEC §8.6): a crash during a read-write open's own truncation or sync leaves a file
/// that recovers to the same state as an uninterrupted open.
#[test]
fn crash_during_recovery_recovers_the_same_state() {
    let mut c = committed_once();
    let tail = c.append(FrameKind::WalChunk, b"acked", NONE).unwrap();
    c.sync().unwrap();
    c.append(FrameKind::WalChunk, &[7; 2000], NONE).unwrap();
    // A torn second append: its header and the start of its payload reached the disk.
    let torn = c.file().bytes()[..usize::try_from(tail.end()).unwrap() + 700].to_vec();

    let clean = Container::open_on(SimFile::from_bytes(torn.clone()), Mode::ReadWrite).unwrap();
    assert!(clean.recovery().truncated_from.is_some());
    let expect = (*clean.superblock(), clean.frames().to_vec(), clean.end());

    // The open makes two mutating calls: the truncation, then the sync.
    for n in 0..2 {
        for fault in [SimFault::FailAt(n), SimFault::ApplyThenFail(n)] {
            for loss in losses() {
                let ctx = format!("{fault:?} {loss:?}");
                let file = SimFile::from_bytes(torn.clone());
                file.set_fault(Some(fault));
                assert!(
                    Container::open_on(file.clone(), Mode::ReadWrite).is_err(),
                    "{ctx}"
                );
                let r = Container::open_on(file.crash(loss), Mode::ReadWrite).unwrap();
                assert_eq!(
                    (*r.superblock(), r.frames().to_vec(), r.end()),
                    expect,
                    "{ctx}"
                );
                assert_eq!(r.file().bytes().len() as u64, r.end(), "{ctx}");
            }
        }
    }
}

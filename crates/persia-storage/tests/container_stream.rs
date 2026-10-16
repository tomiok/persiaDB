//! Streaming appends of the single-file container (ROADMAP 2.4.13; SPEC §4.4 "Streaming and ranged access").
//!
//! Invariants:
//! 1. Equivalence: a frame streamed in any split of its payload leaves the file byte-for-byte as `append` with
//!    `Compression::None` does, gets the same `FrameRef`, and survives a sync and reopen.
//! 2. Poisoning: a writer dropped unfinished, a short `finish`, or a failed write poisons the handle; a reopen then
//!    recovers the state before the streamed frame (its partial bytes are a torn tail), or with the whole frame if the
//!    failed write was its last and landed.
//! 3. Overrun: a chunk past the declared length is an `Encode` error that writes nothing; the writer stays usable.

// Test-only code: unwraps and indexing are the point.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use persia_format::block::Compression;
use persia_format::frame::{FrameKind, ScanEnd};
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Container, ContainerFile, Error, Mode, ReadOnlyReason};
use proptest::prelude::*;

const MAX: usize = 1 << 20;

fn new_sim() -> Container<SimFile> {
    Container::create_on(SimFile::new(), [7; 16]).unwrap()
}

/// A container with one synced WAL frame, to check that a failed stream leaves it as it was.
fn with_one_frame() -> Container<SimFile> {
    let mut c = new_sim();
    c.append(FrameKind::WalChunk, b"before", Compression::None)
        .unwrap();
    c.sync().unwrap();
    c
}

/// Reopens after a crash that keeps every write, and checks only the frame from `with_one_frame` is there.
fn assert_recovers_without_the_stream(file: &SimFile) {
    let c = Container::open_on(file.crash(CrashLoss::Nothing), Mode::ReadWrite).unwrap();
    assert_eq!(c.frames().len(), 1);
    assert_eq!(
        c.read_frame(c.frames()[0].offset, MAX).unwrap().payload,
        b"before"
    );
    assert!(matches!(
        c.recovery().scan_end,
        ScanEnd::TornTail { .. } | ScanEnd::Clean { .. }
    ));
}

proptest! {
    #[test]
    fn streamed_frames_equal_appended_ones(
        payload in proptest::collection::vec(any::<u8>(), 0..3000),
        cuts in proptest::collection::vec(any::<proptest::sample::Index>(), 0..6),
        kind in 0_u8..=6,
    ) {
        let kind = FrameKind::from_u8(kind);
        let mut expected = with_one_frame();
        let want = expected.append(kind, &payload, Compression::None).unwrap();

        let mut c = with_one_frame();
        let mut points: Vec<usize> = cuts.iter().map(|i| i.index(payload.len() + 1)).collect();
        points.push(0);
        points.push(payload.len());
        points.sort_unstable();
        let mut w = c.append_stream(kind, payload.len() as u64).unwrap();
        for pair in points.windows(2) {
            w.write(&payload[pair[0]..pair[1]]).unwrap();
        }
        prop_assert_eq!(w.remaining(), 0);
        let got = w.finish().unwrap();
        prop_assert_eq!(got, want);
        prop_assert_eq!(c.end(), expected.end());
        prop_assert_eq!(c.next_frame_seq(), expected.next_frame_seq());
        prop_assert!(c.has_unsynced());
        prop_assert_eq!(c.file().bytes(), expected.file().bytes());
        c.sync().unwrap();
        let r = Container::open_on(c.file().crash(CrashLoss::All), Mode::ReadOnly).unwrap();
        prop_assert_eq!(r.frames(), c.frames());
        prop_assert_eq!(r.read_frame(got.offset, MAX).unwrap().payload, payload);
    }
}

#[test]
fn dropping_an_unfinished_writer_poisons() {
    let mut c = with_one_frame();
    let mut w = c.append_stream(FrameKind::Segment, 100).unwrap();
    w.write(&[9; 60]).unwrap();
    drop(w);
    assert!(matches!(
        c.append(FrameKind::WalChunk, b"x", Compression::None),
        Err(Error::Poisoned)
    ));
    assert!(matches!(c.sync(), Err(Error::Poisoned)));
    assert_recovers_without_the_stream(c.file());
}

#[test]
fn a_short_finish_poisons() {
    let mut c = with_one_frame();
    let mut w = c.append_stream(FrameKind::Segment, 100).unwrap();
    w.write(&[9; 99]).unwrap();
    assert!(matches!(w.finish(), Err(Error::Encode(_))));
    assert!(matches!(c.sync(), Err(Error::Poisoned)));
    assert_recovers_without_the_stream(c.file());
}

#[test]
fn an_overrun_writes_nothing_and_the_writer_goes_on() {
    let mut c = with_one_frame();
    let mut w = c.append_stream(FrameKind::Segment, 4).unwrap();
    w.write(b"ab").unwrap();
    assert!(matches!(w.write(b"cde"), Err(Error::Encode(_))));
    assert_eq!(w.remaining(), 2);
    w.write(b"cd").unwrap();
    let f = w.finish().unwrap();
    c.sync().unwrap();
    assert_eq!(c.read_frame(f.offset, MAX).unwrap().payload, b"abcd");
}

#[test]
fn a_failed_write_poisons_and_recovers_the_old_state() {
    // Fail each mutating call of the stream in turn: the header, a chunk, the trailer.
    for k in 0..3 {
        let mut c = with_one_frame();
        let first = c.file().mutations();
        c.file().set_fault(Some(SimFault::ApplyThenFail(first + k)));
        let result = (|| {
            let mut w = c.append_stream(FrameKind::Segment, 16)?;
            w.write(&[5; 16])?;
            w.finish()
        })();
        assert!(
            matches!(result, Err(Error::Io { .. })),
            "call {k}: {result:?}"
        );
        assert!(matches!(c.sync(), Err(Error::Poisoned)), "call {k}");
        let file = c.file().clone();
        drop(c);
        for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(k)] {
            // Read-only: a seeded loss may tear the frame out of order, which a read-write open reports (ROADMAP 2.4.10).
            let r = Container::open_on(file.crash(loss), Mode::ReadOnly).unwrap();
            // A failed trailer write that landed completes the frame: the outcome is unknown, never a partial frame.
            match r.frames() {
                [_] => {}
                [_, f] => {
                    assert_eq!(k, 2, "{loss:?}");
                    assert_eq!(r.read_frame(f.offset, MAX).unwrap().payload, [5; 16]);
                }
                other => panic!("call {k}, {loss:?}: {other:?}"),
            }
        }
    }
}

#[test]
fn read_only_and_poisoned_handles_refuse_to_stream() {
    let c = new_sim();
    let file = c.file().clone();
    drop(c);
    let mut r = Container::open_on(file, Mode::ReadOnly).unwrap();
    assert!(matches!(
        r.append_stream(FrameKind::Segment, 1),
        Err(Error::ReadOnly {
            reason: ReadOnlyReason::OpenedReadOnly
        })
    ));
    let mut c = new_sim();
    drop(c.append_stream(FrameKind::Segment, 1).unwrap());
    assert!(matches!(
        c.append_stream(FrameKind::Segment, 1),
        Err(Error::Poisoned)
    ));
    assert!(matches!(
        new_sim().append_stream(FrameKind::Segment, u64::MAX),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(new_sim().file().file_len().unwrap(), 12288);
}

//! Streaming appends interleaved with the other writes of a container (ROADMAP 2.4.13; SPEC §4.4 "Streaming and
//! ranged access", §4.5). `container_stream.rs` checks one streamed frame at a time.
//!
//! Invariant: any sequence of `append`, `append_stream` (any split of the payload, empty chunks included), `sync`,
//! `commit` (with a Manifest appended either way) and reopen (after a crash keeping all or none of the unsynced
//! writes) leaves the container exactly as the same sequence with every streamed frame replaced by
//! `append(.., Compression::None)`: after every step the current and durable file bytes, the frames, `end`,
//! `next_frame_seq`, `has_unsynced` and the active superblock are equal; at the end a read-only reopen reads the same
//! payloads.
//!
//! Seeded (`PROPTEST_RNG_SEED` and `PROPTEST_CASES` override the defaults).

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFile};
use persia_storage::container::{Commit, Container, FrameRef, Mode};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::RngSeed;

const MAX: usize = 1 << 20;
const SEED: u64 = 0x5045_5253_4941_0413; // "PERSIA", 2.4.13

#[cfg(not(feature = "slow"))]
const SCALE: u32 = 1;
#[cfg(feature = "slow")]
const SCALE: u32 = 32;

fn config(cases: u32) -> ProptestConfig {
    let env = ProptestConfig::default();
    ProptestConfig {
        cases: if std::env::var_os("PROPTEST_CASES").is_some() {
            env.cases
        } else {
            cases * SCALE
        },
        rng_seed: if std::env::var_os("PROPTEST_RNG_SEED").is_some() {
            env.rng_seed
        } else {
            RngSeed::Fixed(SEED)
        },
        ..env
    }
}

#[derive(Debug, Clone)]
enum Op {
    /// A plain append, the same on both sides (compressed sometimes).
    Append(u8, Vec<u8>, bool),
    /// Streamed in the pieces `cuts` makes on the side under test, appended uncompressed on the model.
    Stream(u8, Vec<u8>, Vec<Index>),
    Sync,
    /// Appends a Manifest (streamed when the flag is set) and commits with `wal_start` at a picked frame or the end.
    Commit(bool, Index),
    /// Drops both handles and reopens read-write after a crash that keeps all (`true`) or none of the unsynced writes.
    Reopen(bool),
}

fn payload() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        4 => proptest::collection::vec(any::<u8>(), 0..100),
        1 => (any::<u8>(), 0_usize..20_000).prop_map(|(b, n)| vec![b; n]),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (1_u8..=6, payload(), any::<bool>()).prop_map(|(k, p, z)| Op::Append(k, p, z)),
        4 => (1_u8..=6, payload(), proptest::collection::vec(any::<Index>(), 0..8))
            .prop_map(|(k, p, c)| Op::Stream(k, p, c)),
        2 => Just(Op::Sync),
        2 => (any::<bool>(), any::<Index>()).prop_map(|(s, i)| Op::Commit(s, i)),
        1 => any::<bool>().prop_map(Op::Reopen),
    ]
}

fn stream(c: &mut Container<SimFile>, kind: FrameKind, payload: &[u8], cuts: &[Index]) -> FrameRef {
    let mut points: Vec<usize> = cuts.iter().map(|i| i.index(payload.len() + 1)).collect();
    points.extend([0, payload.len()]);
    points.sort_unstable();
    let mut w = c.append_stream(kind, payload.len() as u64).unwrap();
    for pair in points.windows(2) {
        w.write(&payload[pair[0]..pair[1]]).unwrap();
    }
    assert_eq!(w.remaining(), 0);
    w.finish().unwrap()
}

fn assert_same(got: &Container<SimFile>, want: &Container<SimFile>, step: usize) {
    assert!(
        got.file().bytes() == want.file().bytes(),
        "step {step}: file bytes differ"
    );
    assert!(
        got.file().durable_bytes() == want.file().durable_bytes(),
        "step {step}: durable bytes differ"
    );
    assert_eq!(got.frames(), want.frames(), "step {step}");
    assert_eq!(got.end(), want.end(), "step {step}");
    assert_eq!(got.next_frame_seq(), want.next_frame_seq(), "step {step}");
    assert_eq!(got.has_unsynced(), want.has_unsynced(), "step {step}");
    assert_eq!(got.superblock(), want.superblock(), "step {step}");
}

fn run(ops: &[Op]) {
    let mut got = Container::create_on(SimFile::new(), [5; 16]).unwrap();
    let mut want = Container::create_on(SimFile::new(), [5; 16]).unwrap();
    for (step, op) in ops.iter().enumerate() {
        match op {
            Op::Append(k, p, zstd) => {
                let compression = if *zstd {
                    Compression::Zstd { level: 1 }
                } else {
                    Compression::None
                };
                let kind = FrameKind::from_u8(*k);
                let a = got.append(kind, p, compression).unwrap();
                let b = want.append(kind, p, compression).unwrap();
                assert_eq!(a, b, "step {step}");
            }
            Op::Stream(k, p, cuts) => {
                let kind = FrameKind::from_u8(*k);
                let a = stream(&mut got, kind, p, cuts);
                let b = want.append(kind, p, Compression::None).unwrap();
                assert_eq!(a, b, "step {step}");
            }
            Op::Sync => {
                got.sync().unwrap();
                want.sync().unwrap();
            }
            Op::Commit(streamed, at) => {
                let body = format!("manifest {step}").into_bytes();
                let m = if *streamed {
                    stream(&mut got, FrameKind::Manifest, &body, &[])
                } else {
                    got.append(FrameKind::Manifest, &body, Compression::None)
                        .unwrap()
                };
                let n = want
                    .append(FrameKind::Manifest, &body, Compression::None)
                    .unwrap();
                assert_eq!(m, n, "step {step}");
                // A frame from the committed `wal_start` on, or the end.
                let starts: Vec<u64> = want
                    .frames()
                    .iter()
                    .map(|f| f.offset)
                    .filter(|&o| o >= want.superblock().wal_start)
                    .chain([want.end()])
                    .collect();
                let wal_start = starts[at.index(starts.len())];
                let commit = Commit {
                    manifest: m,
                    wal_start,
                    fence_token: 0,
                };
                got.commit(commit).unwrap();
                want.commit(commit).unwrap();
            }
            Op::Reopen(keep) => {
                let loss = if *keep {
                    CrashLoss::Nothing
                } else {
                    CrashLoss::All
                };
                let (a, b) = (got.file().crash(loss), want.file().crash(loss));
                assert!(a.bytes() == b.bytes(), "step {step}: crashed files differ");
                drop((got, want));
                got = Container::open_on(a, Mode::ReadWrite).unwrap();
                want = Container::open_on(b, Mode::ReadWrite).unwrap();
            }
        }
        assert_same(&got, &want, step);
    }
    got.sync().unwrap();
    let r = Container::open_on(got.file().crash(CrashLoss::All), Mode::ReadOnly).unwrap();
    assert_eq!(r.frames(), got.frames());
    for f in r.frames() {
        assert_eq!(
            r.read_frame(f.offset, MAX).unwrap().payload,
            want.read_frame(f.offset, MAX).unwrap().payload
        );
    }
}

proptest! {
    #![proptest_config(config(128))]

    #[test]
    fn streamed_and_appended_frames_interleave_like_appends_only(
        ops in proptest::collection::vec(op(), 0..24),
    ) {
        run(&ops);
    }
}

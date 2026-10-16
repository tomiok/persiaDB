//! Property tests of the single-file container (SPEC §4.4, §4.5, §4.5.1, §8.6; ROADMAP 2.4.1-2.4.4).
//!
//! Invariants:
//! 1. Model: after any sequence of appends, syncs, commits and reopens (clean or after a power loss, read-write or
//!    read-only), the container equals a reference model of the committed superblock, the frames from `wal_start`
//!    with their payloads, the uncommitted frames after `log_end`, and `next_frame_seq`. A clean read-write reopen
//!    makes the frames it finds after `log_end` durable (SPEC §4.5.1: "Valid frames after `log_end` are durable but
//!    not committed").
//! 2. Crash anywhere: a fault at any mutating call of such a sequence, then a crash that keeps all, none, or a
//!    seeded subset of the unsynced writes, recovers the last acknowledged commit or the one in flight, never a mix;
//!    every acknowledged frame survives; recovered frames are a prefix of what was appended; a read-only open always
//!    succeeds; and a read-write recovery (unless `CorruptTail`) accepts a further commit.
//! 3. Truncation at every 8-byte offset at or after `log_end` is a clean end or a torn tail, never `CorruptTail`;
//!    below `log_end` it is corruption in both modes, and the file is never modified.
//! 4. Any single damaged byte in `wal_start..log_end` is corruption in both modes, and the file is never modified.
//!
//! Every runner is seeded (`PROPTEST_RNG_SEED` and `PROPTEST_CASES` override the defaults).

// Test-only code: unwraps, panics, indexing and casts are the point.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::panic,
    clippy::expect_used,
    clippy::similar_names
)]

use persia_format::Writer;
use persia_format::block::Compression;
use persia_format::frame::{FrameKind, ScanEnd, encode_frame};
use persia_format::superblock::{FIRST_FRAME_OFFSET, SLOT_LEN, Slot, Superblock};
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Commit, Container, Error, FrameRef, Mode};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::RngSeed;

const MAX: usize = 1 << 20;
const NONE: Compression = Compression::None;
const SEED: u64 = 0x5045_5253_4941_0204; // "PERSIA", 2.4

#[cfg(not(feature = "slow"))]
const SCALE: u32 = 1;
#[cfg(feature = "slow")]
const SCALE: u32 = 32;

/// A seeded config with `cases` (times [`SCALE`]) unless the environment says otherwise.
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

// ---------------------------------------------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------------------------------------------

/// A position in a list whose length is known only when the operation runs: `0` is the first element,
/// `u16::MAX` the last. Shrinks toward the first.
#[derive(Debug, Clone, Copy)]
struct Pick(u16);

impl Pick {
    const FIRST: Self = Self(0);
    const LAST: Self = Self(u16::MAX);

    fn get<T>(self, items: &[T]) -> &T {
        assert!(!items.is_empty() && items.len() <= 1 << 16);
        &items[(usize::from(self.0) * items.len()) >> 16]
    }
}

#[derive(Debug, Clone)]
enum Op {
    Append {
        kind: FrameKind,
        payload: Vec<u8>,
        compression: Compression,
    },
    Sync,
    /// Commits a Manifest (a fresh one if `fresh` is set or none is available, else one picked among those from
    /// `wal_start` on) with a `wal_start` picked among the valid ones.
    Commit {
        fresh: Option<Vec<u8>>,
        manifest: Pick,
        wal_start: Pick,
        fence_token: u64,
    },
    /// A commit whose `wal_start` is inside a frame: rejected, nothing changes.
    BadCommit {
        frame: Pick,
    },
    /// Drops the handle and reopens read-write: on the same file (a process restart that keeps the page cache) or
    /// on what survives a power loss (only synced writes).
    ReopenRw {
        power_loss: bool,
    },
    /// Opens a read-only handle beside the writer and checks it; the writer goes on.
    ReopenRo {
        power_loss: bool,
    },
}

fn kind() -> impl Strategy<Value = FrameKind> {
    prop_oneof![
        4 => Just(FrameKind::WalChunk),
        2 => Just(FrameKind::Segment),
        2 => Just(FrameKind::Manifest),
        1 => Just(FrameKind::TombstoneSidecar),
        1 => Just(FrameKind::Padding),
    ]
}

fn payload() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => prop::collection::vec(any::<u8>(), 0..48),
        1 => prop::collection::vec(any::<u8>(), 0..600),
        // Compressible: zstd frames get exercised.
        1 => (any::<u8>(), 0..1500_usize).prop_map(|(b, n)| vec![b; n]),
    ]
}

fn compression() -> impl Strategy<Value = Compression> {
    prop_oneof![
        3 => Just(NONE),
        1 => (1_i32..=3).prop_map(|level| Compression::Zstd { level }),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (kind(), payload(), compression()).prop_map(|(kind, payload, compression)| Op::Append {
            kind,
            payload,
            compression,
        }),
        2 => Just(Op::Sync),
        3 => (
            prop::option::of(payload()),
            any::<u16>().prop_map(Pick),
            any::<u16>().prop_map(Pick),
            prop_oneof![Just(0_u64), any::<u64>()],
        )
            .prop_map(|(fresh, manifest, wal_start, fence_token)| Op::Commit {
                fresh,
                manifest,
                wal_start,
                fence_token,
            }),
        1 => any::<u16>().prop_map(Pick).prop_map(|frame| Op::BadCommit { frame }),
        1 => any::<bool>().prop_map(|power_loss| Op::ReopenRw { power_loss }),
        1 => any::<bool>().prop_map(|power_loss| Op::ReopenRo { power_loss }),
    ]
}

fn ops(max: usize) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op(), 1..max)
}

fn loss() -> impl Strategy<Value = CrashLoss> {
    prop_oneof![
        1 => Just(CrashLoss::All),
        1 => Just(CrashLoss::Nothing),
        2 => any::<u64>().prop_map(CrashLoss::Seeded),
    ]
}

// ---------------------------------------------------------------------------------------------------------------
// Reference model
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct MFrame {
    frame: FrameRef,
    payload: Vec<u8>,
}

/// A committed state: the superblock and the payload of the Manifest it names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Committed {
    sb: Superblock,
    manifest: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct Model {
    committed: Committed,
    /// Every frame from the committed `wal_start` to `end`.
    frames: Vec<MFrame>,
    end: u64,
    next_frame_seq: u64,
    /// Frames ending at or before it are durable (acknowledged by a sync, a commit, or a read-write recovery).
    synced_end: u64,
}

impl Model {
    fn new() -> Self {
        Self {
            committed: Committed {
                sb: Superblock {
                    log_end: FIRST_FRAME_OFFSET,
                    wal_start: FIRST_FRAME_OFFSET,
                    next_frame_seq: 1,
                    ..Superblock::default()
                },
                manifest: None,
            },
            frames: Vec::new(),
            end: FIRST_FRAME_OFFSET,
            next_frame_seq: 1,
            synced_end: FIRST_FRAME_OFFSET,
        }
    }

    /// The frame an append of these arguments must produce (SPEC §4.4 layout, ADR-0013 numbering).
    fn expected_append(&self, kind: FrameKind, payload: &[u8], compression: Compression) -> MFrame {
        let mut w = Writer::new();
        encode_frame(&mut w, kind, self.next_frame_seq, payload, compression).unwrap();
        MFrame {
            frame: FrameRef {
                offset: self.end,
                len: w.len() as u64,
                kind,
                frame_seq: self.next_frame_seq,
            },
            payload: payload.to_vec(),
        }
    }

    fn push(&mut self, f: MFrame) {
        self.end = f.frame.end();
        self.next_frame_seq += 1;
        self.frames.push(f);
    }

    fn sync(&mut self) {
        self.synced_end = self.end;
    }

    fn manifests(&self) -> Vec<MFrame> {
        self.frames
            .iter()
            .filter(|f| f.frame.kind == FrameKind::Manifest)
            .cloned()
            .collect()
    }

    /// Valid `wal_start` values: a frame start from the committed `wal_start` on, or the end (SPEC §4.5).
    fn wal_starts(&self) -> Vec<u64> {
        self.frames
            .iter()
            .map(|f| f.frame.offset)
            .chain([self.end])
            .collect()
    }

    fn commit_target(&self, manifest: &MFrame, wal_start: u64, fence_token: u64) -> Committed {
        Committed {
            sb: Superblock {
                seq: self.committed.sb.seq + 1,
                manifest_off: manifest.frame.offset,
                manifest_len: manifest.frame.len,
                log_end: self.end,
                wal_start,
                fence_token,
                next_frame_seq: self.next_frame_seq,
            },
            manifest: Some(manifest.payload.clone()),
        }
    }

    fn apply_commit(&mut self, target: Committed) {
        let wal_start = target.sb.wal_start;
        self.frames.retain(|f| f.frame.offset >= wal_start);
        self.committed = target;
        self.synced_end = self.end;
    }

    /// What a power loss keeps: the synced frames.
    fn lose_unsynced(&mut self) {
        let synced_end = self.synced_end;
        let before = self.frames.len();
        self.frames.retain(|f| f.frame.end() <= synced_end);
        self.next_frame_seq -= (before - self.frames.len()) as u64;
        self.end = synced_end;
    }

    fn uncommitted(&self) -> impl Iterator<Item = &MFrame> {
        let log_end = self.committed.sb.log_end;
        self.frames
            .iter()
            .filter(move |f| f.frame.offset >= log_end)
    }
}

/// The container equals the model in everything a caller can observe.
fn check(c: &Container<SimFile>, m: &Model, ctx: &str) {
    let sb = m.committed.sb;
    assert_eq!(*c.superblock(), sb, "{ctx}: superblock");
    let slot = if sb.seq.is_multiple_of(2) {
        Slot::A
    } else {
        Slot::B
    };
    assert_eq!(c.active_slot(), slot, "{ctx}: active slot");
    let refs: Vec<FrameRef> = m.frames.iter().map(|f| f.frame).collect();
    assert_eq!(c.frames(), refs.as_slice(), "{ctx}: frames");
    let uncommitted: Vec<FrameRef> = m.uncommitted().map(|f| f.frame).collect();
    assert_eq!(
        c.uncommitted_frames(),
        uncommitted.as_slice(),
        "{ctx}: uncommitted frames"
    );
    assert_eq!(c.end(), m.end, "{ctx}: end");
    assert_eq!(
        c.next_frame_seq(),
        m.next_frame_seq,
        "{ctx}: next_frame_seq"
    );
    for f in &m.frames {
        let got = c.read_frame(f.frame.offset, MAX).unwrap();
        assert_eq!(got.frame, f.frame, "{ctx}");
        assert!(got.payload == f.payload, "{ctx}: payload of {:?}", f.frame);
    }
    if let Some(manifest) = &m.committed.manifest {
        let got = c.read_frame(sb.manifest_off, MAX).unwrap();
        assert!(got.payload == *manifest, "{ctx}: manifest payload");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Running operations against the container and the model
// ---------------------------------------------------------------------------------------------------------------

/// A writer, its model, and what was in flight when an operation failed.
struct Run {
    c: Container<SimFile>,
    /// The file of the run now: `c`'s, or the one a failed read-write reopen was recovering.
    file: SimFile,
    model: Model,
    /// The fault to inject, as an index into the mutating calls of every file the run used, in order.
    fault: Option<SimFault>,
    /// Mutating calls made by files that power-loss reopens replaced.
    consumed: u64,
    /// The frame a failed append may have written.
    maybe: Option<MFrame>,
    /// The commit a failed commit may have published.
    inflight: Option<Committed>,
}

impl Run {
    fn new(fault: Option<SimFault>) -> Self {
        let file = SimFile::new();
        let mut run = Self {
            c: Container::create_on(file.clone(), [3; 16]).unwrap(),
            file,
            model: Model::new(),
            fault,
            consumed: 0,
            maybe: None,
            inflight: None,
        };
        run.arm(&run.file.clone());
        run
    }

    /// All mutating calls so far.
    fn mutations(&self) -> u64 {
        self.consumed + self.file.mutations()
    }

    /// Arms the global fault on `file`, the current file of the run.
    fn arm(&mut self, file: &SimFile) {
        let local = |n: u64| n.checked_sub(self.consumed);
        let fault = match self.fault {
            Some(SimFault::FailAt(n)) => local(n).map(SimFault::FailAt),
            Some(SimFault::ApplyThenFail(n)) => local(n).map(SimFault::ApplyThenFail),
            None => None,
        };
        file.set_fault(fault);
    }

    fn append(
        &mut self,
        kind: FrameKind,
        payload: &[u8],
        compression: Compression,
    ) -> Result<MFrame, Error> {
        let f = self.model.expected_append(kind, payload, compression);
        self.maybe = Some(f.clone());
        let got = self.c.append(kind, payload, compression)?;
        self.maybe = None;
        assert_eq!(got, f.frame, "append");
        self.model.push(f.clone());
        Ok(f)
    }

    /// Runs one operation; on `Err` the model is the last acknowledged state, `maybe` and `inflight` what the
    /// failed operation may have done.
    fn step(&mut self, op: &Op, ctx: &str) -> Result<(), Error> {
        self.maybe = None;
        self.inflight = None;
        match op {
            Op::Append {
                kind,
                payload,
                compression,
            } => {
                self.append(*kind, payload, *compression)?;
            }
            Op::Sync => {
                self.c.sync()?;
                self.model.sync();
            }
            Op::Commit {
                fresh,
                manifest,
                wal_start,
                fence_token,
            } => {
                let manifests = self.model.manifests();
                let m = match (fresh, manifests.is_empty()) {
                    (None, false) => manifest.get(&manifests).clone(),
                    (fresh, _) => {
                        let payload = fresh.clone().unwrap_or_else(|| b"manifest".to_vec());
                        self.append(FrameKind::Manifest, &payload, NONE)?
                    }
                };
                let wal_start = *wal_start.get(&self.model.wal_starts());
                let target = self.model.commit_target(&m, wal_start, *fence_token);
                self.inflight = Some(target.clone());
                self.c.commit(Commit {
                    manifest: m.frame,
                    wal_start,
                    fence_token: *fence_token,
                })?;
                self.inflight = None;
                self.model.apply_commit(target);
            }
            Op::BadCommit { frame } => {
                let Some(f) =
                    (!self.model.frames.is_empty()).then(|| frame.get(&self.model.frames))
                else {
                    return Ok(());
                };
                let manifest = self.model.manifests().first().map_or(f.frame, |m| m.frame);
                let err = self
                    .c
                    .commit(Commit {
                        manifest,
                        // Frames are at least 32 bytes long: 8 bytes in is never a frame start or the end.
                        wal_start: f.frame.offset + 8,
                        fence_token: 0,
                    })
                    .unwrap_err();
                assert!(matches!(err, Error::InvalidInput(_)), "{ctx}: {err}");
                check(
                    &self.c,
                    &self.model,
                    &format!("{ctx}: after a rejected commit"),
                );
            }
            Op::ReopenRw { power_loss } => {
                let file = if *power_loss {
                    self.consumed += self.file.mutations();
                    let file = self.file.crash(CrashLoss::All);
                    self.model.lose_unsynced();
                    file
                } else {
                    self.c.file().clone()
                };
                self.arm(&file);
                self.file = file.clone();
                // Recovery may write (truncate a torn tail, sync): an injected fault fails it like any write.
                self.c = Container::open_on(file, Mode::ReadWrite)?;
                // SPEC §4.5.1: after a read-write recovery, the valid frames after `log_end` are durable.
                self.model.sync();
                check(&self.c, &self.model, &format!("{ctx}: reopened read-write"));
            }
            Op::ReopenRo { power_loss } => {
                let mut model = self.model.clone();
                let file = if *power_loss {
                    model.lose_unsynced();
                    self.c.file().crash(CrashLoss::All)
                } else {
                    self.c.file().crash(CrashLoss::Nothing)
                };
                let ro = Container::open_on(file.clone(), Mode::ReadOnly)
                    .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"));
                check(&ro, &model, &format!("{ctx}: reopened read-only"));
                assert!(ro.file().bytes() == file.bytes(), "{ctx}: a reader wrote");
            }
        }
        Ok(())
    }

    /// Runs `ops` with no fault; every one must succeed.
    fn run_all(ops: &[Op]) -> Self {
        let mut run = Self::new(None);
        for (i, op) in ops.iter().enumerate() {
            let ctx = format!("op {i} {op:?}");
            run.step(op, &ctx).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            check(&run.c, &run.model, &ctx);
        }
        run
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Model-based
// ---------------------------------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(config(64))]

    /// Invariant 1: the container equals the model after every operation and every reopen.
    #[test]
    fn container_matches_the_model(ops in ops(40)) {
        let mut run = Run::run_all(&ops);
        // A final reopen of each kind sees the same state.
        for (i, op) in [Op::ReopenRo { power_loss: false }, Op::ReopenRw { power_loss: false },
                        Op::Sync, Op::ReopenRo { power_loss: true }, Op::ReopenRw { power_loss: true }]
            .iter()
            .enumerate()
        {
            run.step(op, &format!("final {i} {op:?}")).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 2. Crash anywhere
// ---------------------------------------------------------------------------------------------------------------

/// The recovered container holds a commit-boundary state of the run, every acknowledged frame, and nothing but a
/// prefix of the appended frames. Returns the recovered committed state.
fn check_recovered(c: &Container<SimFile>, run: &Run, ctx: &str) -> Committed {
    let sb = *c.superblock();
    let acked = &run.model.committed;
    let committed = [Some(acked), run.inflight.as_ref()]
        .into_iter()
        .flatten()
        .find(|k| k.sb == sb)
        .unwrap_or_else(|| {
            panic!(
                "{ctx}: recovered {sb:?}, neither the acknowledged {:?} nor the in-flight {:?}",
                acked.sb,
                run.inflight.as_ref().map(|k| k.sb)
            )
        });
    assert!(
        sb.seq >= acked.sb.seq,
        "{ctx}: an acknowledged commit was lost"
    );
    if let Some(manifest) = &committed.manifest {
        let got = c.read_frame(sb.manifest_off, MAX).unwrap();
        assert!(got.payload == *manifest, "{ctx}: manifest payload");
    }
    // Everything the run appended (or may have), from the recovered wal_start on, in order.
    let appended: Vec<&MFrame> = run
        .model
        .frames
        .iter()
        .chain(run.maybe.iter())
        .filter(|f| f.frame.offset >= sb.wal_start)
        .collect();
    let got = c.frames();
    assert!(
        got.len() <= appended.len(),
        "{ctx}: recovered {} frames, only {} appended",
        got.len(),
        appended.len()
    );
    for (g, e) in got.iter().zip(&appended) {
        assert_eq!(
            *g, e.frame,
            "{ctx}: recovered frames are a prefix of the appended ones"
        );
        let payload = c.read_frame(g.offset, MAX).unwrap().payload;
        assert!(payload == e.payload, "{ctx}: payload of {g:?}");
    }
    let durable_end = run.model.synced_end.max(sb.log_end);
    let durable = appended
        .iter()
        .filter(|f| f.frame.end() <= durable_end)
        .count();
    assert!(
        got.len() >= durable,
        "{ctx}: an acknowledged frame was lost: {} recovered, {durable} acknowledged (durable up to {durable_end})",
        got.len()
    );
    let last_uncommitted = c.uncommitted_frames().last();
    assert_eq!(
        c.next_frame_seq(),
        last_uncommitted.map_or(sb.next_frame_seq, |f| f.frame_seq + 1),
        "{ctx}: next_frame_seq"
    );
    assert_eq!(
        c.end(),
        last_uncommitted.map_or(sb.log_end, FrameRef::end),
        "{ctx}: end"
    );
    committed.clone()
}

/// The recovered writer accepts a commit, which a power loss right after it keeps.
fn check_writable(mut c: Container<SimFile>, ctx: &str) {
    let sb = *c.superblock();
    let m = c
        .append(FrameKind::Manifest, b"after the crash", NONE)
        .unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: sb.wal_start,
        fence_token: sb.fence_token,
    })
    .unwrap_or_else(|e| panic!("{ctx}: commit after recovery: {e}"));
    let frames = c.frames().to_vec();
    let r = Container::open_on(c.file().crash(CrashLoss::All), Mode::ReadWrite)
        .unwrap_or_else(|e| panic!("{ctx}: reopen after the new commit: {e}"));
    assert_eq!(r.superblock().seq, sb.seq + 1, "{ctx}");
    assert_eq!(r.frames(), frames.as_slice(), "{ctx}");
    assert_eq!(
        r.read_frame(r.superblock().manifest_off, MAX)
            .unwrap()
            .payload,
        b"after the crash",
        "{ctx}"
    );
}

/// Invariant 2 for one sequence, fault and loss.
fn crash_case(ops: &[Op], at: Index, apply: bool, loss: CrashLoss) {
    let dry = Run::run_all(ops);
    // `create_on` makes the first two calls; faults go after them.
    let first = 2;
    let total = dry.mutations();
    if total <= first {
        return;
    }
    let n = first + at.index((total - first) as usize) as u64;
    let fault = if apply {
        SimFault::ApplyThenFail(n)
    } else {
        SimFault::FailAt(n)
    };
    let mut run = Run::new(Some(fault));
    let mut failed = None;
    for (i, op) in ops.iter().enumerate() {
        let ctx = format!("{fault:?} op {i} {op:?}");
        if let Err(err) = run.step(op, &ctx) {
            assert!(matches!(err, Error::Io { .. }), "{ctx}: {err}");
            failed = Some(ctx);
            break;
        }
    }
    let ctx = format!("{} {loss:?}", failed.expect("the fault fired"));
    assert!(run.file.crashed(), "{ctx}");

    let image = run.file.crash(loss).bytes();
    let ro_file = SimFile::from_bytes(image.clone());
    let ro = Container::open_on(ro_file.clone(), Mode::ReadOnly)
        .unwrap_or_else(|e| panic!("{ctx}: read-only recovery: {e}"));
    let ro_state = check_recovered(&ro, &run, &format!("{ctx} (read-only)"));
    assert!(ro_file.bytes() == image, "{ctx}: a reader wrote");

    let rw_file = SimFile::from_bytes(image.clone());
    match Container::open_on(rw_file.clone(), Mode::ReadWrite) {
        Ok(rw) => {
            let rw_state = check_recovered(&rw, &run, &format!("{ctx} (read-write)"));
            assert_eq!(
                rw_state, ro_state,
                "{ctx}: both modes recover the same commit"
            );
            assert_eq!(
                rw.frames(),
                ro.frames(),
                "{ctx}: both modes find the same frames"
            );
            assert!(
                rw_file.durable_bytes() == rw_file.bytes(),
                "{ctx}: recovery left unsynced changes"
            );
            check_writable(rw, &ctx);
        }
        // Unsynced appends that reached the disk out of order (SPEC §4.4): only a partial crash does that.
        Err(Error::CorruptTail { .. }) => {
            assert!(matches!(loss, CrashLoss::Seeded(_)), "{ctx}");
            assert!(
                rw_file.bytes() == image,
                "{ctx}: CorruptTail modified the file"
            );
        }
        Err(err) => panic!("{ctx}: read-write recovery: {err}"),
    }
}

proptest! {
    #![proptest_config(config(128))]

    /// Invariant 2: a crash at any mutating call recovers a commit boundary and every acknowledged frame.
    #[test]
    fn crash_anywhere_recovers_a_commit_boundary(
        ops in ops(24),
        at in any::<Index>(),
        apply in any::<bool>(),
        loss in loss(),
    ) {
        crash_case(&ops, at, apply, loss);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 3. Truncation, 4. single-byte damage
// ---------------------------------------------------------------------------------------------------------------

/// A container built by `ops`, with `tail` synced, uncommitted WAL frames after `log_end`.
///
/// Read-write reopens of the same file are left out: what they make durable is invariant 1's business (see
/// `frames_found_by_read_write_recovery_are_durable`), and these properties are about the bytes, not about how they
/// got there.
fn built(ops: &[Op], tail: &[Vec<u8>]) -> Run {
    let ops: Vec<Op> = ops
        .iter()
        .filter(|op| !matches!(op, Op::ReopenRw { power_loss: false }))
        .cloned()
        .collect();
    let mut run = Run::run_all(&ops);
    for payload in tail {
        run.append(FrameKind::WalChunk, payload, NONE).unwrap();
    }
    run.step(&Op::Sync, "tail sync").unwrap();
    run
}

fn tail() -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(prop::collection::vec(any::<u8>(), 0..80), 0..4)
}

/// Opens `bytes` in `mode` and expects corruption that leaves the file untouched (SPEC §8.6).
fn expect_corruption(bytes: &[u8], mode: Mode, ctx: &str) {
    let file = SimFile::from_bytes(bytes.to_vec());
    let err = Container::open_on(file.clone(), mode).unwrap_err();
    assert!(err.is_corruption(), "{ctx} {mode:?}: {err}");
    assert!(
        file.bytes() == bytes,
        "{ctx} {mode:?}: the file was modified"
    );
    assert_eq!(file.mutations(), 0, "{ctx} {mode:?}: the file was written");
}

/// Invariant 3 for one container.
fn truncation_case(run: &Run) {
    let bytes = run.c.file().bytes();
    let sb = *run.c.superblock();
    let len = bytes.len() as u64;
    assert_eq!(len, run.model.end);
    for cut in (sb.log_end..=len).step_by(8) {
        let ctx = format!("cut at {cut} (log_end {}, len {len})", sb.log_end);
        let image = &bytes[..cut as usize];
        let frames: Vec<FrameRef> = run
            .model
            .frames
            .iter()
            .map(|f| f.frame)
            .filter(|f| f.end() <= cut)
            .collect();
        let boundary = frames
            .last()
            .map_or(sb.log_end, FrameRef::end)
            .max(sb.log_end);
        let torn = boundary < cut;

        let ro_file = SimFile::from_bytes(image.to_vec());
        let ro = Container::open_on(ro_file.clone(), Mode::ReadOnly)
            .unwrap_or_else(|e| panic!("{ctx}: read-only: {e}"));
        let rw_file = SimFile::from_bytes(image.to_vec());
        let rw = Container::open_on(rw_file.clone(), Mode::ReadWrite)
            .unwrap_or_else(|e| panic!("{ctx}: read-write: {e}"));
        for c in [&ro, &rw] {
            assert_eq!(*c.superblock(), sb, "{ctx}");
            assert_eq!(c.frames(), frames.as_slice(), "{ctx}: {:?}", c.mode());
            assert_eq!(c.end(), boundary, "{ctx}");
            let end = &c.recovery().scan_end;
            if torn {
                assert!(
                    matches!(end, ScanEnd::TornTail { offset, .. } if *offset == boundary),
                    "{ctx}: {end:?}"
                );
            } else {
                assert_eq!(*end, ScanEnd::Clean { offset: cut }, "{ctx}");
            }
        }
        assert!(ro_file.bytes() == image, "{ctx}: a reader wrote");
        assert_eq!(ro.recovery().truncated_from, None, "{ctx}");
        assert_eq!(rw.recovery().truncated_from, torn.then_some(cut), "{ctx}");
        assert!(
            rw_file.durable_bytes() == image[..boundary as usize],
            "{ctx}: a writer truncates a torn tail durably, and only it"
        );
    }
    for cut in (0..sb.log_end).step_by(8) {
        let ctx = format!("cut at {cut} below log_end {}", sb.log_end);
        for mode in [Mode::ReadOnly, Mode::ReadWrite] {
            expect_corruption(&bytes[..cut as usize], mode, &ctx);
        }
    }
}

/// Invariant 4 for one damaged byte.
fn damage_case(run: &Run, at: u64, mask: u8) {
    let sb = *run.c.superblock();
    assert!((sb.wal_start..sb.log_end).contains(&at));
    let mut bytes = run.c.file().bytes();
    bytes[at as usize] ^= mask;
    let ctx = format!(
        "byte {at} ^ {mask:#04x} in {}..{}",
        sb.wal_start, sb.log_end
    );
    for mode in [Mode::ReadOnly, Mode::ReadWrite] {
        let file = SimFile::from_bytes(bytes.clone());
        let err = Container::open_on(file.clone(), mode).unwrap_err();
        assert!(
            matches!(err, Error::Format(_)) && err.is_corruption(),
            "{ctx} {mode:?}: {err}"
        );
        assert!(
            file.bytes() == bytes,
            "{ctx} {mode:?}: the file was modified"
        );
        assert_eq!(file.mutations(), 0, "{ctx} {mode:?}: the file was written");
    }
}

proptest! {
    #![proptest_config(config(16))]

    /// Invariant 3: truncation after `log_end` is a clean end or a torn tail; below it, corruption.
    #[test]
    fn truncation_after_log_end_is_torn_and_below_is_corruption(ops in ops(16), tail in tail()) {
        truncation_case(&built(&ops, &tail));
    }
}

proptest! {
    #![proptest_config(config(128))]

    /// Invariant 4: one damaged byte in the committed frames is corruption in both modes.
    #[test]
    fn damaged_committed_byte_is_corruption(
        ops in ops(24),
        tail in tail(),
        at in any::<Index>(),
        mask in 1_u8..=255,
    ) {
        let mut run = built(&ops, &tail);
        if run.c.superblock().log_end == run.c.superblock().wal_start {
            // Commit a fresh Manifest with wal_start at it, so the committed range is not empty.
            let commit = Op::Commit { fresh: Some(b"m".to_vec()), manifest: Pick::FIRST, wal_start: Pick::FIRST, fence_token: 0 };
            run.step(&commit, "non-empty committed range").unwrap();
        }
        let sb = *run.c.superblock();
        let span = sb.log_end - sb.wal_start;
        prop_assert!(span > 0);
        damage_case(&run, sb.wal_start + at.index(span as usize) as u64, mask);
    }
}

/// A fixed container with every frame kind, compression, a moved `wal_start` and an uncommitted tail.
fn fixed() -> Run {
    let ops = [
        Op::Append {
            kind: FrameKind::WalChunk,
            payload: b"dropped by wal_start".to_vec(),
            compression: NONE,
        },
        Op::Commit {
            fresh: Some(b"m1".to_vec()),
            manifest: Pick::FIRST,
            wal_start: Pick::LAST,
            fence_token: 0,
        },
        Op::Append {
            kind: FrameKind::Segment,
            payload: vec![7; 700],
            compression: Compression::Zstd { level: 1 },
        },
        // Incompressible: stored as is although zstd was asked for.
        Op::Append {
            kind: FrameKind::Segment,
            payload: (0..700_u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
                .collect(),
            compression: Compression::Zstd { level: 1 },
        },
        Op::Append {
            kind: FrameKind::WalChunk,
            payload: b"put 1".to_vec(),
            compression: NONE,
        },
        Op::Append {
            kind: FrameKind::TombstoneSidecar,
            payload: vec![1, 2, 3],
            compression: NONE,
        },
        Op::Append {
            kind: FrameKind::Padding,
            payload: Vec::new(),
            compression: NONE,
        },
        Op::Sync,
        Op::Commit {
            fresh: Some(b"m2".to_vec()),
            manifest: Pick::FIRST,
            wal_start: Pick::FIRST,
            fence_token: 9,
        },
    ];
    built(&ops, &[b"tail 1".to_vec(), b"tail 2".to_vec()])
}

/// Invariant 3, exhaustively, on the fixed container.
#[test]
fn truncation_of_a_fixed_container_at_every_offset() {
    truncation_case(&fixed());
}

/// Invariant 4, exhaustively, on the fixed container: every committed byte, two masks.
#[test]
fn every_committed_byte_of_a_fixed_container_is_checked() {
    let run = fixed();
    let sb = *run.c.superblock();
    assert!(sb.log_end - sb.wal_start > 800, "{sb:?}");
    for at in sb.wal_start..sb.log_end {
        for mask in [0x01, 0x80] {
            damage_case(&run, at, mask);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Durability of frames found by a read-write recovery (minimal cases of what the properties above found)
// ---------------------------------------------------------------------------------------------------------------

/// A process appends a WAL frame and a Manifest without syncing and dies; the OS keeps its writes in the page cache.
/// A new process opens the same file read-write.
fn restarted_with_unsynced_frames() -> (Container<SimFile>, FrameRef) {
    let mut c = Container::create_on(SimFile::new(), [5; 16]).unwrap();
    c.append(FrameKind::WalChunk, b"put 1", NONE).unwrap();
    let m = c.append(FrameKind::Manifest, b"manifest 1", NONE).unwrap();
    let file = c.file().clone();
    drop(c);
    let r = Container::open_on(file, Mode::ReadWrite).unwrap();
    assert_eq!(r.uncommitted_frames().len(), 2);
    (r, m)
}

/// SPEC §4.5.1: "Valid frames after `log_end` are durable but not committed". A read-write recovery reports frames
/// that are only in the page cache, and the new handle's `sync` does nothing for them (it has appended nothing), so a
/// power loss then loses frames the handle presented as durable.
#[test]
fn frames_found_by_read_write_recovery_are_durable() {
    let (mut r, _) = restarted_with_unsynced_frames();
    r.sync().unwrap();
    let found = r.frames().to_vec();
    let after = Container::open_on(r.file().crash(CrashLoss::All), Mode::ReadOnly).unwrap();
    assert_eq!(
        after.frames(),
        found.as_slice(),
        "a power loss after recovery and sync lost frames the recovery reported"
    );
}

/// SPEC §4.5 step 2 (frames durable before the slot names them): committing a Manifest found by recovery, with no
/// new append, skips the data sync, so the slot write can reach the disk before the frames it covers. The OS may
/// write the slot page back first; a power loss then leaves a superblock pointing past the end of the file, and the
/// container no longer opens in either mode.
#[test]
fn commit_of_recovered_frames_never_publishes_a_slot_before_them() {
    let (mut r, m) = restarted_with_unsynced_frames();
    let file = r.file().clone();
    let slot = r.active_slot().other();
    // Die right after the slot write, before its sync.
    file.set_fault(Some(SimFault::FailAt(file.mutations() + 1)));
    let wal_start = r.superblock().wal_start;
    let result = r.commit(Commit {
        manifest: m,
        wal_start,
        fence_token: 0,
    });
    assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
    // The page cache wrote the slot back, not the frames: a legal crash image.
    let mut image = file.durable_bytes();
    let at = slot.offset() as usize;
    image[at..at + SLOT_LEN].copy_from_slice(&file.bytes()[at..at + SLOT_LEN]);
    for mode in [Mode::ReadOnly, Mode::ReadWrite] {
        let c = Container::open_on(SimFile::from_bytes(image.clone()), mode)
            .unwrap_or_else(|e| panic!("{mode:?}: the container no longer opens: {e}"));
        assert!(c.superblock().seq <= 1, "{mode:?}");
    }
}

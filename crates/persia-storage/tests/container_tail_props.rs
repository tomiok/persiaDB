//! Property test of a read-write open over random damage after `log_end` (ROADMAP 2.4.10; SPEC §4.4, §4.5.1;
//! ADR-0014), against a reference classification written from the ADR. `container_tail.rs` has the hand-built
//! cases and the crash-at-every-step test.
//!
//! Reference (independent of `tail.rs`): the scan from `log_end` expecting the committed `next_frame_seq` is a
//! `FrameScanner` over the file's bytes. If it ends in corruption at `o`, with `e` the `frame_seq` expected there,
//! the bytes `o..end` are searched at every 8-aligned offset: a frame that `Frame::decode` accepts there is
//! *valid*; a valid frame with `frame_seq >= e`, or one rejected only for unknown `flags` (a newer writer's frame,
//! SPEC §4.4), decides case (a) at its offset; a valid one below `e` is stale and skipped whole; any other non-zero
//! 8-byte word makes the tail garbage. The implementation's read budget (`Undecided`) is not modeled: the generators
//! never approach it, and `a_tail_too_costly_to_search_is_never_discarded` covers it.
//!
//! Invariants:
//! 1. Clean scan: opens, nothing changes. Torn tail: truncated at the stop, no quarantine file.
//! 2. (a) A valid frame with `frame_seq >= e` at an aligned offset `>= o`: a `Repair::Safe` open fails with
//!    `CorruptTail { offset: o, acknowledged: Some(first such offset) }` and changes nothing (same bytes, no new file).
//! 3. (b) Only zeros and stale frames: the container is truncated at `o`, durably; no quarantine file.
//! 4. (c) Otherwise: exactly one quarantine file, named `<name>.quarantine.<o as 16 hex digits>.*`, holding exactly
//!    the bytes `o..end`; the container is truncated at `o`; both survive a crash that loses every unsynced write.
//! 5. `Repair::Quarantine` always opens (unless the frame at the stop is unsupported): as (c) for (a), else as Safe.
//! 6. A read-only open never changes anything and reports the reference scan end; an `open_on` (no path) read-write
//!    open behaves as Safe except that (c) fails with `CorruptTail { acknowledged: None }` and changes nothing.
//! 7. After a successful read-write open, a reopen scans clean to the same end.
//!
//! Seeded (`PROPTEST_RNG_SEED` and `PROPTEST_CASES` override the defaults).

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::panic,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::single_match_else,
    clippy::manual_assert_eq // comparing large byte vectors: a mismatch would print them whole
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_format::block::Compression;
use persia_format::checksum::crc32c;
use persia_format::frame::{FRAME_MAGIC, Frame, FrameKind, FrameScanner, ScanEnd, encode_frame};
use persia_format::{Reader, Writer};
use persia_storage::container::sim::{CrashLoss, SimFile};
use persia_storage::container::{Commit, Container, ContainerFile, Error, Mode, Repair};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::RngSeed;

const NONE: Compression = Compression::None;
const DB: &str = "/db/app.persia";
const SEED: u64 = 0x5045_5253_4941_0410; // "PERSIA", 2.4.10

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

// ---------------------------------------------------------------------------------------------------------------
// The container before damage
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Build {
    /// WAL payload lengths committed before `log_end`.
    committed: Vec<usize>,
    /// WAL payload lengths appended and synced after `log_end`.
    tail: Vec<usize>,
    zstd_last: bool,
}

fn payload_len() -> impl Strategy<Value = usize> {
    // Sometimes larger than the 64 KiB chunk of the search and the copy.
    prop_oneof![6 => 0_usize..200, 3 => 200_usize..5000, 1 => 60_000_usize..140_000]
}

fn build() -> impl Strategy<Value = Build> {
    (
        proptest::collection::vec(payload_len(), 0..3),
        proptest::collection::vec(payload_len(), 0..5),
        any::<bool>(),
    )
        .prop_map(|(committed, tail, zstd_last)| Build {
            committed,
            tail,
            zstd_last,
        })
}

fn payload(len: usize, salt: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + salt * 7 + 1) as u8).collect()
}

/// The container's bytes, `log_end`, the committed `next_frame_seq`, and every frame's `[start, end)`.
struct Built {
    bytes: Vec<u8>,
    log_end: u64,
    next_seq: u64,
    frames: Vec<(usize, usize)>,
}

fn build_container(b: &Build) -> Built {
    let mut c = Container::create_on(SimFile::new(), [3; 16]).unwrap();
    let mut frames = Vec::new();
    for (i, &len) in b.committed.iter().enumerate() {
        let f = c
            .append(FrameKind::WalChunk, &payload(len, i), NONE)
            .unwrap();
        frames.push((f.offset as usize, f.end() as usize));
    }
    c.sync().unwrap();
    let m = c.append(FrameKind::Manifest, b"manifest", NONE).unwrap();
    frames.push((m.offset as usize, m.end() as usize));
    c.commit(Commit {
        manifest: m,
        wal_start: c.superblock().wal_start,
        fence_token: 0,
    })
    .unwrap();
    let (log_end, next_seq) = (c.superblock().log_end, c.superblock().next_frame_seq);
    for (i, &len) in b.tail.iter().enumerate() {
        let compression = if b.zstd_last && i + 1 == b.tail.len() {
            Compression::Zstd { level: 1 }
        } else {
            NONE
        };
        let f = c
            .append(FrameKind::WalChunk, &payload(len, i + 10), compression)
            .unwrap();
        frames.push((f.offset as usize, f.end() as usize));
    }
    c.sync().unwrap();
    Built {
        bytes: c.file().bytes(),
        log_end,
        next_seq,
        frames,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Damage after `log_end`
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Seq {
    /// `frame_seq` = `k`: stale or not depending on where the scan stops.
    Small(u64),
    Max,
    Any(u64),
}

#[derive(Debug, Clone)]
enum Damage {
    /// Lost sectors (or a smaller range): zeros.
    Zero(Index, usize),
    FlipBit(Index, u8),
    Overwrite(Index, Vec<u8>),
    Truncate(Index),
    AppendZeros(usize),
    AppendBytes(Vec<u8>),
    /// A freshly encoded valid frame at an aligned offset (the file grows if needed).
    Plant(Index, Seq, usize),
    /// A valid frame but for unknown `flags` (a newer writer's), CRC fixed up, at an aligned offset.
    PlantUnknownFlags(Index, u64),
    /// A copy of an existing frame (committed or not) at an aligned offset.
    Copy(Index, Index),
    /// A copy of an existing frame right at the end, after zero padding to an aligned length.
    CopyAtEnd(Index, usize),
}

fn damage() -> impl Strategy<Value = Damage> {
    let i = any::<Index>;
    prop_oneof![
        3 => (i(), prop_oneof![Just(512_usize), 1_usize..2000]).prop_map(|(a, n)| Damage::Zero(a, n)),
        2 => (i(), 0_u8..8).prop_map(|(a, b)| Damage::FlipBit(a, b)),
        1 => (i(), proptest::collection::vec(any::<u8>(), 1..64)).prop_map(|(a, v)| Damage::Overwrite(a, v)),
        1 => i().prop_map(Damage::Truncate),
        1 => prop_oneof![1_usize..100, 65_000_usize..140_000].prop_map(Damage::AppendZeros),
        1 => proptest::collection::vec(any::<u8>(), 1..64).prop_map(Damage::AppendBytes),
        2 => (
            i(),
            prop_oneof![4 => (0_u64..10).prop_map(Seq::Small), 1 => Just(Seq::Max), 1 => any::<u64>().prop_map(Seq::Any)],
            prop_oneof![0_usize..100, 70_000_usize..70_100],
        )
            .prop_map(|(a, s, n)| Damage::Plant(a, s, n)),
        1 => (i(), 0_u64..10).prop_map(|(a, s)| Damage::PlantUnknownFlags(a, s)),
        2 => (i(), i()).prop_map(|(f, a)| Damage::Copy(f, a)),
        1 => (i(), prop_oneof![Just(0_usize), 1_usize..100, 65_530_usize..65_560])
            .prop_map(|(f, z)| Damage::CopyAtEnd(f, z)),
    ]
}

/// An offset in `log_end..=len` picked by `at`.
fn pick(at: Index, log_end: usize, len: usize) -> usize {
    log_end + at.index(len - log_end + 1)
}

/// The same, rounded down to 8 (never below `log_end`, which is aligned).
fn pick_aligned(at: Index, log_end: usize, len: usize) -> usize {
    pick(at, log_end, len) & !7
}

fn put(bytes: &mut Vec<u8>, at: usize, data: &[u8]) {
    if bytes.len() < at + data.len() {
        bytes.resize(at + data.len(), 0);
    }
    bytes[at..at + data.len()].copy_from_slice(data);
}

fn apply(bytes: &mut Vec<u8>, built: &Built, d: &Damage) {
    let log_end = built.log_end as usize;
    let len = bytes.len();
    match *d {
        Damage::Zero(at, n) => {
            let from = pick(at, log_end, len);
            let to = (from + n).min(len);
            bytes[from..to].fill(0);
        }
        Damage::FlipBit(at, bit) if len > log_end => {
            bytes[log_end + at.index(len - log_end)] ^= 1 << bit;
        }
        Damage::FlipBit(..) => {}
        Damage::Overwrite(at, ref v) => put(bytes, pick(at, log_end, len), v),
        Damage::Truncate(at) => bytes.truncate(pick(at, log_end, len)),
        Damage::AppendZeros(n) => bytes.resize(len + n, 0),
        Damage::AppendBytes(ref v) => bytes.extend_from_slice(v),
        Damage::Plant(at, ref seq, n) => {
            let seq = match *seq {
                Seq::Small(k) => k,
                Seq::Max => u64::MAX,
                Seq::Any(s) => s,
            };
            let at = pick_aligned(at, log_end, len);
            let mut w = Writer::with_base_offset(at as u64);
            encode_frame(&mut w, FrameKind::WalChunk, seq, &payload(n, 99), NONE).unwrap();
            put(bytes, at, w.as_slice());
        }
        Damage::PlantUnknownFlags(at, seq) => {
            let at = pick_aligned(at, log_end, len);
            let mut w = Writer::with_base_offset(at as u64);
            encode_frame(&mut w, FrameKind::WalChunk, seq, &payload(24, 98), NONE).unwrap();
            let mut frame = w.into_inner();
            frame[5] |= 0x80; // an unknown flag bit
            let n = frame.len();
            let crc = crc32c(&frame[4..n - 8]);
            frame[n - 8..n - 4].copy_from_slice(&crc.to_le_bytes());
            put(bytes, at, &frame);
        }
        Damage::Copy(f, at) => {
            let (s, e) = built.frames[f.index(built.frames.len())];
            let frame = built.bytes[s..e].to_vec();
            put(bytes, pick_aligned(at, log_end, len), &frame);
        }
        Damage::CopyAtEnd(f, zeros) => {
            let (s, e) = built.frames[f.index(built.frames.len())];
            let frame = built.bytes[s..e].to_vec();
            let at = (len + zeros + 7) & !7;
            put(bytes, at, &frame);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Reference (ADR-0014)
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expected {
    Clean,
    Torn(u64),
    Unsupported,
    /// (a): stop, first valid frame with `frame_seq >= e`.
    Acknowledged(u64, u64),
    /// (b)
    Discardable(u64),
    /// (c)
    Garbage(u64),
}

/// What `Frame::decode` says of a frame at `at`.
enum AtWord {
    /// Valid: `(frame_seq, total_len)`.
    Valid(u64, u64),
    /// Valid but for unknown `flags`: a newer writer's frame, never discarded (SPEC §4.4).
    Unsupported,
    Invalid,
}

fn frame_at(bytes: &[u8], at: usize) -> AtWord {
    match Frame::decode(&mut Reader::with_base_offset(&bytes[at..], at as u64)) {
        Ok(f) => AtWord::Valid(f.frame_seq(), f.total_len()),
        Err(persia_format::Error::Unsupported { .. }) => AtWord::Unsupported,
        Err(_) => AtWord::Invalid,
    }
}

fn classify(bytes: &[u8], o: u64, expected_seq: u64) -> Expected {
    let mut at = o as usize;
    let mut garbage = false;
    while at < bytes.len() {
        if bytes[at..].starts_with(&FRAME_MAGIC) {
            match frame_at(bytes, at) {
                AtWord::Valid(seq, total) if seq < expected_seq => {
                    at += total as usize;
                    continue;
                }
                AtWord::Valid(..) | AtWord::Unsupported => {
                    return Expected::Acknowledged(o, at as u64);
                }
                AtWord::Invalid => {}
            }
        }
        let word = &bytes[at..(at + 8).min(bytes.len())];
        garbage |= word.iter().any(|&b| b != 0);
        at += 8;
    }
    if garbage {
        Expected::Garbage(o)
    } else {
        Expected::Discardable(o)
    }
}

/// The scan end the open must report, and what a read-write open must do.
fn reference(bytes: &[u8], log_end: u64, next_seq: u64) -> (ScanEnd, Expected) {
    let mut s = FrameScanner::new(&bytes[log_end as usize..], log_end).expect_frame_seq(next_seq);
    s.by_ref().for_each(drop);
    let end = s.end().unwrap().clone();
    let expected = match end {
        ScanEnd::Clean { .. } => Expected::Clean,
        ScanEnd::TornTail { offset, .. } => Expected::Torn(offset),
        ScanEnd::Unsupported { .. } => Expected::Unsupported,
        ScanEnd::Corrupt { offset, .. } => classify(bytes, offset, s.next_frame_seq().unwrap()),
    };
    (end, expected)
}

// ---------------------------------------------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------------------------------------------

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

/// A file system holding `bytes` at `DB`, durably.
fn fs_with(bytes: &[u8]) -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    let f = fs.create_new(Path::new(DB)).unwrap();
    f.write_all_at(bytes, 0).unwrap();
    f.sync_data().unwrap();
    drop(f);
    fs.sync_dir(Path::new("/db")).unwrap();
    fs
}

fn quarantine_files(fs: &SimFs) -> Vec<(PathBuf, Vec<u8>)> {
    fs.files()
        .into_iter()
        .filter(|(p, _)| p.to_string_lossy().contains(".quarantine."))
        .collect()
}

fn other_files(fs: &SimFs) -> Vec<PathBuf> {
    fs.files()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p != Path::new(DB) && !p.to_string_lossy().contains(".quarantine."))
        .collect()
}

/// What a read-write open did, as observed.
fn check_rw(
    bytes: &[u8],
    want_end: &ScanEnd,
    expected: &Expected,
    repair: Repair,
) -> Result<(), TestCaseError> {
    let len = bytes.len() as u64;
    let fs = fs_with(bytes);
    let result = Container::open_in_with(vfs(&fs), DB, Mode::ReadWrite, repair);
    let now = fs.read(Path::new(DB)).unwrap();
    let qs = quarantine_files(&fs);
    prop_assert_eq!(other_files(&fs), Vec::<PathBuf>::new(), "stray files");
    // Under `Repair::Quarantine`, (a) behaves like (c).
    let expected = match (expected, repair) {
        (Expected::Acknowledged(o, _), Repair::Quarantine) => Expected::Garbage(*o),
        (e, _) => e.clone(),
    };
    let truncated_to = |c: &Container<SimFsFile>, o: u64| -> Result<(), TestCaseError> {
        prop_assert_eq!(c.end(), o);
        prop_assert_eq!(c.recovery().truncated_from, Some(len));
        prop_assert_eq!(&c.recovery().scan_end, want_end);
        prop_assert_eq!(&now[..], &bytes[..o as usize]);
        Ok(())
    };
    match expected {
        Expected::Clean => {
            let c = result.map_err(|e| TestCaseError::fail(format!("clean: {e}")))?;
            prop_assert_eq!(c.end(), len);
            prop_assert_eq!(c.recovery().truncated_from, None);
            prop_assert_eq!(&c.recovery().scan_end, want_end);
            prop_assert_eq!(&now[..], bytes);
            prop_assert!(qs.is_empty());
        }
        Expected::Torn(o) | Expected::Discardable(o) => {
            let c = result.map_err(|e| TestCaseError::fail(format!("{expected:?}: {e}")))?;
            truncated_to(&c, o)?;
            prop_assert_eq!(&c.recovery().quarantined, &None);
            prop_assert!(qs.is_empty(), "{:?}: quarantined", expected);
        }
        Expected::Garbage(o) => {
            let c = result.map_err(|e| TestCaseError::fail(format!("{expected:?}: {e}")))?;
            truncated_to(&c, o)?;
            prop_assert_eq!(qs.len(), 1);
            let (path, copy) = &qs[0];
            prop_assert_eq!(c.recovery().quarantined.as_ref(), Some(path));
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let prefix = format!("app.persia.quarantine.{o:016x}.");
            prop_assert!(name.starts_with(&prefix), "{}", name);
            prop_assert!(
                copy.as_slice() == &bytes[o as usize..],
                "quarantine copy differs"
            );
            drop(c);
            // Durable: both survive a crash that loses every unsynced write.
            let after = fs.crash(CrashLoss::All);
            prop_assert!(after.read(path).unwrap().as_slice() == &bytes[o as usize..]);
            prop_assert!(after.read(Path::new(DB)).unwrap().as_slice() == &bytes[..o as usize]);
        }
        Expected::Acknowledged(o, at) => {
            match result {
                Err(Error::CorruptTail {
                    offset,
                    acknowledged,
                    ..
                }) => {
                    prop_assert_eq!((offset, acknowledged), (o, Some(at)));
                }
                other => {
                    return Err(TestCaseError::fail(format!(
                        "want CorruptTail at {o} acknowledged {at}, got {other:?}"
                    )));
                }
            }
            prop_assert_eq!(&now[..], bytes, "changed");
            prop_assert!(qs.is_empty());
        }
        Expected::Unsupported => {
            prop_assert!(
                matches!(result, Err(Error::Format(_))),
                "{:?}",
                result.err()
            );
            prop_assert_eq!(&now[..], bytes);
            prop_assert!(qs.is_empty());
            return Ok(());
        }
    }
    // Invariant 7.
    let after = fs.read(Path::new(DB)).unwrap();
    if !matches!(expected, Expected::Acknowledged(..)) {
        let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite)
            .map_err(|e| TestCaseError::fail(format!("reopen: {e}")))?;
        let clean = matches!(c.recovery().scan_end, ScanEnd::Clean { .. });
        prop_assert!(clean, "reopen not clean");
        prop_assert_eq!(c.end(), after.len() as u64);
    }
    Ok(())
}

fn check_ro(bytes: &[u8], want_end: &ScanEnd) -> Result<(), TestCaseError> {
    let fs = fs_with(bytes);
    let result = Container::open_in(vfs(&fs), DB, Mode::ReadOnly);
    match want_end {
        ScanEnd::Unsupported { .. } => prop_assert!(matches!(result, Err(Error::Format(_)))),
        _ => {
            let c = result.map_err(|e| TestCaseError::fail(format!("read-only: {e}")))?;
            prop_assert_eq!(&c.recovery().scan_end, want_end);
            prop_assert_eq!(c.end(), want_end.offset());
        }
    }
    prop_assert!(fs.read(Path::new(DB)).unwrap().as_slice() == bytes);
    prop_assert_eq!(fs.files().len(), 1);
    Ok(())
}

fn check_open_on(bytes: &[u8], expected: &Expected) -> Result<(), TestCaseError> {
    let file = SimFile::from_bytes(bytes.to_vec());
    let result = Container::open_on(file.clone(), Mode::ReadWrite);
    match *expected {
        Expected::Clean => prop_assert!(result.is_ok()),
        Expected::Torn(o) | Expected::Discardable(o) => {
            prop_assert_eq!(
                result
                    .map_err(|e| TestCaseError::fail(e.to_string()))?
                    .end(),
                o
            );
            prop_assert!(file.bytes().as_slice() == &bytes[..o as usize]);
            return Ok(());
        }
        Expected::Garbage(o) => {
            let ok = matches!(result, Err(Error::CorruptTail { offset, acknowledged: None, .. }) if offset == o);
            prop_assert!(ok, "want CorruptTail without a path: {:?}", result.err());
        }
        Expected::Acknowledged(o, at) => {
            let ok = matches!(result, Err(Error::CorruptTail { offset, acknowledged: Some(a), .. }) if offset == o && a == at);
            prop_assert!(ok, "want CorruptTail acknowledged: {:?}", result.err());
        }
        Expected::Unsupported => prop_assert!(matches!(result, Err(Error::Format(_)))),
    }
    prop_assert!(file.bytes().as_slice() == bytes, "changed");
    Ok(())
}

fn check_case(b: &Build, damages: &[Damage]) -> Result<Expected, TestCaseError> {
    let built = build_container(b);
    let mut bytes = built.bytes.clone();
    for d in damages {
        apply(&mut bytes, &built, d);
    }
    let (want_end, expected) = reference(&bytes, built.log_end, built.next_seq);
    check_ro(&bytes, &want_end)?;
    check_open_on(&bytes, &expected)?;
    check_rw(&bytes, &want_end, &expected, Repair::Safe)?;
    check_rw(&bytes, &want_end, &expected, Repair::Quarantine)?;
    Ok(expected)
}

proptest! {
    #![proptest_config(config(96))]

    #[test]
    fn rw_open_after_damage_matches_adr_0014(
        b in build(),
        damages in proptest::collection::vec(damage(), 1..4),
    ) {
        check_case(&b, &damages)?;
    }
}

/// The generators reach every outcome of the reference, so the property above is not vacuous.
#[test]
fn the_generator_reaches_every_outcome() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::new(config(1));
    let strategy = (build(), proptest::collection::vec(damage(), 1..4));
    let mut seen = [false; 6];
    for _ in 0..400 {
        let (b, damages) = strategy.new_tree(&mut runner).unwrap().current();
        let built = build_container(&b);
        let mut bytes = built.bytes.clone();
        for d in &damages {
            apply(&mut bytes, &built, d);
        }
        let i = match reference(&bytes, built.log_end, built.next_seq).1 {
            Expected::Clean => 0,
            Expected::Torn(_) => 1,
            Expected::Unsupported => 2,
            Expected::Acknowledged(..) => 3,
            Expected::Discardable(_) => 4,
            Expected::Garbage(_) => 5,
        };
        seen[i] = true;
    }
    // Unsupported needs unknown flags with a matching CRC, which random damage almost never makes.
    assert_eq!(
        [seen[0], seen[1], seen[3], seen[4], seen[5]],
        [true; 5],
        "{seen:?}"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Hand-picked layouts around the 64 KiB chunk boundary of the search
// ---------------------------------------------------------------------------------------------------------------

/// One piece of a tail laid out from the stop `o`, at an offset relative to `o`.
enum Piece {
    /// A valid `WalChunk` frame with this `frame_seq` and payload.
    Frame(u64, Vec<u8>),
    /// One non-zero byte.
    Byte,
}

/// A committed container (Manifest `frame_seq` 1) whose synced tail frame (`frame_seq` 2) was lost entirely
/// (zeros), with `pieces` written over the zeroed tail, which is `len` bytes long (pieces past it are cut). Returns the bytes and `o`.
fn layout(len: usize, pieces: &[(usize, Piece)]) -> (Vec<u8>, usize) {
    let built = build_container(&Build {
        committed: vec![],
        tail: vec![100],
        zstd_last: false,
    });
    assert_eq!(built.next_seq, 2);
    let o = built.log_end as usize;
    let mut bytes = built.bytes[..o].to_vec();
    bytes.resize(o + len, 0);
    for (rel, piece) in pieces {
        let at = o + rel;
        match piece {
            Piece::Frame(seq, payload) => {
                let mut w = Writer::with_base_offset(at as u64);
                encode_frame(&mut w, FrameKind::WalChunk, *seq, payload, NONE).unwrap();
                put(&mut bytes, at, w.as_slice());
            }
            Piece::Byte => put(&mut bytes, at, &[0x5A]),
        }
    }
    bytes.truncate(o + len);
    (bytes, o)
}

fn encoded(seq: u64, payload: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    encode_frame(&mut w, FrameKind::WalChunk, seq, payload, NONE).unwrap();
    w.into_inner()
}

/// Runs every open of `check_case` on `bytes` and checks the reference agrees with `want` (relative offsets).
fn check_layout(name: &str, bytes: &[u8], o: usize, want: &Expected) {
    let (end, expected) = reference(bytes, 12288 + 40, 2);
    assert_eq!(&expected, want, "{name}: the reference itself");
    let run = || -> Result<(), TestCaseError> {
        check_ro(bytes, &end)?;
        check_open_on(bytes, &expected)?;
        check_rw(bytes, &end, &expected, Repair::Safe)?;
        check_rw(bytes, &end, &expected, Repair::Quarantine)
    };
    run().unwrap_or_else(|e| panic!("{name} (o = {o}): {e}"));
}

const K64: usize = 64 * 1024;

#[test]
fn candidates_and_stale_frames_across_the_search_chunk_boundary() {
    let o = (12288 + 40) as u64;
    let stale = || Piece::Frame(1, vec![7; 40]); // 72 bytes, `frame_seq` below the expected 2
    let fresh = |seq| Piece::Frame(seq, vec![8; 16]);
    let cases: Vec<(&str, usize, Vec<(usize, Piece)>, Expected)> = vec![
        (
            "stale straddling",
            K64 + 200,
            vec![(K64 - 8, stale())],
            Expected::Discardable(o),
        ),
        (
            "stale ending on the boundary",
            K64 + 200,
            vec![(K64 - 72, stale())],
            Expected::Discardable(o),
        ),
        (
            "fresh after a straddling stale",
            K64 + 200,
            vec![(K64 - 8, stale()), (K64 + 64, fresh(2))],
            Expected::Acknowledged(o, o + K64 as u64 + 64),
        ),
        (
            "fresh straddling",
            K64 + 200,
            vec![(K64 - 8, fresh(9))],
            Expected::Acknowledged(o, o + K64 as u64 - 8),
        ),
        (
            "fresh in the third chunk",
            2 * K64 + 64,
            vec![(2 * K64, fresh(u64::MAX))],
            Expected::Acknowledged(o, o + 2 * K64 as u64),
        ),
        (
            "garbage after a straddling stale",
            K64 + 200,
            vec![(K64 - 8, stale()), (K64 + 199, Piece::Byte)],
            Expected::Garbage(o),
        ),
        (
            "a torn candidate is garbage",
            K64 + 16,
            vec![(K64 - 8, fresh(9))],
            Expected::Garbage(o),
        ),
        (
            // A stale frame whose payload is a fresh frame at an aligned offset: skipped whole, never a candidate.
            "fresh frame inside a stale one",
            K64 + 200,
            vec![(K64 - 16, Piece::Frame(1, encoded(50, &[1; 24])))],
            Expected::Discardable(o),
        ),
        (
            "stale run across two chunks then fresh",
            2 * K64 + 200,
            vec![
                (K64 - 24, Piece::Frame(1, vec![3; K64])),
                (2 * K64 + 16, fresh(2)),
            ],
            Expected::Acknowledged(o, o + 2 * K64 as u64 + 16),
        ),
    ];
    for (name, len, pieces, want) in cases {
        let (bytes, o) = layout(len, &pieces);
        check_layout(name, &bytes, o, &want);
    }
}

#[cfg(feature = "slow")]
proptest! {
    #![proptest_config(config(64))]

    #[test]
    fn slow_rw_open_after_heavy_damage_matches_adr_0014(
        b in build(),
        damages in proptest::collection::vec(damage(), 1..10),
    ) {
        check_case(&b, &damages)?;
    }
}

/// A stale block exposed by a crash during a file extension holds many small old frames back to back (ADR-0014
/// case 2): zeros and stale frames only, so a read-write open must truncate it without quarantine, however many
/// frames there are.
#[test]
fn many_small_stale_frames_are_discarded() {
    let o = (12288 + 40) as u64;
    for count in [10_usize, 500, 2000] {
        let frame = encoded(1, &[7; 16]); // 48 bytes, `frame_seq` 1 < 2
        let len = 64 + count * frame.len();
        let pieces: Vec<(usize, Piece)> = (0..count)
            .map(|i| (64 + i * frame.len(), Piece::Frame(1, vec![7; 16])))
            .collect();
        let (bytes, o_rel) = layout(len, &pieces);
        check_layout(
            &format!("{count} stale frames"),
            &bytes,
            o_rel,
            &Expected::Discardable(o),
        );
    }
}

/// A hostile tail where every other aligned word starts a frame header that claims to run exactly to the end, with
/// a wrong CRC: checking each would read the tail quadratically, so the search gives up (`Undecided`). Nothing is
/// known, so a `Repair::Safe` open refuses without naming a frame and changes nothing, `open_on` refuses the same
/// way, and `Repair::Quarantine` quarantines the whole tail.
#[test]
fn a_tail_too_costly_to_search_is_never_discarded() {
    let built = build_container(&Build {
        committed: vec![],
        tail: vec![100],
        zstd_last: false,
    });
    let o = built.log_end as usize;
    let tail = 256 * 1024;
    let mut bytes = built.bytes[..o].to_vec();
    bytes.resize(o + tail, 0);
    let end = bytes.len();
    // No magic at `o`: the scan stops there in corruption, not at a frame running to the end.
    bytes[o..o + 16].fill(0xFF);
    for at in (o + 16..end - 32).step_by(16) {
        bytes[at..at + 4].copy_from_slice(&FRAME_MAGIC);
        bytes[at + 4] = 3; // WalChunk
        let payload_len = (end - at - 32) as u64; // a multiple of 8: no padding, ends exactly at `end`
        bytes[at + 8..at + 16].copy_from_slice(&payload_len.to_le_bytes());
    }
    // The reference, which has no read budget, calls it garbage.
    assert_eq!(
        reference(&bytes, o as u64, 2).1,
        Expected::Garbage(o as u64)
    );

    let fs = fs_with(&bytes);
    let err = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap_err();
    assert!(
        matches!(err, Error::CorruptTail { offset, acknowledged: None, .. } if offset == o as u64),
        "{err}"
    );
    assert!(fs.read(Path::new(DB)).unwrap() == bytes, "changed");
    assert_eq!(quarantine_files(&fs), []);

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
    assert!(file.bytes() == bytes);

    let c = Container::open_in_with(vfs(&fs), DB, Mode::ReadWrite, Repair::Quarantine).unwrap();
    assert_eq!(c.end(), o as u64);
    let q = c.recovery().quarantined.clone().unwrap();
    assert!(fs.read(&q).unwrap() == bytes[o..]);
}

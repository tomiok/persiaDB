//! `StreamScanner` against `FrameScanner` on logs built to stress chunking (ROADMAP 2.4.12; SPEC §4.4).
//! `frame_props.rs` (invariant 7) compares the two on randomly mutated logs with one random chunk length per case and
//! bases below 2^40; this suite complements it.
//!
//! Invariants:
//! 1. Equivalence under every chunking: for one log, *every* chunk length in `1..=40` and around the header/trailer
//!    widths and powers of two (63, 64, 65, 4096) gives the same frames (offset, length, kind, flags, `frame_seq`),
//!    the same `ScanEnd` (error kind and offset included), the same final `offset()` and the same `next_frame_seq()`
//!    as a `FrameScanner` over the same bytes, under each `frame_seq` check (off, contiguous, expect `first`).
//!    Logs hold frames much larger than the chunk whose damage is targeted where a chunk boundary matters: a
//!    non-zero padding byte or `frame_seq` or unknown flag with the CRC fixed up (so the error is past the CRC
//!    check), a flipped CRC bit, a non-zero `reserved` byte, a payload bit, a lost sector, a truncation, and zero or
//!    non-zero bytes after the last frame.
//! 2. Bases near `u64::MAX`: the same holds when the log ends just below `u64::MAX` (no overflow, no saturation
//!    divergence), with `frame_seq` runs that reach `u64::MAX` (out of range under a check).
//! 3. Read discipline: every read lies inside `base..base + len`, is non-empty and at most
//!    `max(chunk, FRAME_HEADER_LEN)` bytes.
//! 4. Retry: a read error at any read is returned as is, leaves the scanner unchanged, and retrying with a good
//!    reader yields exactly the frames and end of an undisturbed scan.
//!
//! `StreamScanner::reframe` (ROADMAP 2.4.17; SPEC §4.4, §4.6), on the same damaged logs, at every original frame
//! start, the end of the log, past it and arbitrary (unaligned) offsets, under every chunk length above:
//! 5. Equivalence: it returns exactly what `check_frame` returns (header, or error kind and offset), and that is
//!    what `Frame::decode` over the bytes from the offset to the end gives.
//! 6. Copy: on `Ok`, the bytes written are exactly `Frame::reframe(.., frame_seq)` of the decoded frame,
//!    `total_len` of them (compressed frames copied as stored). On a frame whose header fits but that is invalid
//!    or unsupported, exactly the copy's header and stored payload are written (never the trailer). Nothing is
//!    written for fewer than 32 bytes, a bad magic or a frame that runs past the end.
//! 7. Read discipline: reads are non-empty, at most `max(chunk, 24)` bytes, contiguous from the offset, and cover
//!    exactly the frame's extent once (only its 24-byte header for a bad magic or a frame that does not fit,
//!    nothing for fewer than 32 bytes). Writes are non-empty and at most `max(chunk, 24)` bytes.
//! 8. Errors of `read` or `write` at any call are returned as is, and nothing is called after them.
//! 9. Bases near `u64::MAX` (and an offset saturated at `u64::MAX`): the same, without overflow.

#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::cast_possible_truncation
)]

use std::cell::RefCell;

use persia_format::block::Compression;
use persia_format::checksum::crc32c;
use persia_format::frame::{
    FLAG_ZSTD, FRAME_HEADER_LEN, FRAME_MAGIC, Frame, FrameHeader, FrameKind, FrameScanner,
    MIN_FRAME_LEN, ScanEnd, StreamScanner, encode_frame,
};
use persia_format::{Error, Reader, Writer};
use proptest::prelude::*;
use proptest::sample::Index;

/// `cases` unless `PROPTEST_CASES` says otherwise.
fn config(cases: u32) -> ProptestConfig {
    let env = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_some() {
        env
    } else {
        ProptestConfig { cases, ..env }
    }
}

/// Chunk lengths every case is scanned with.
fn chunk_lens() -> impl Iterator<Item = usize> {
    (1..=40).chain([63, 64, 65, 4096])
}

#[derive(Debug, Clone, Copy)]
enum SeqMode {
    Off,
    Contiguous,
    Expect(u64),
}

/// `(offset, total_len, kind, flags, frame_seq)` of a frame either scanner yields.
type Seen = (u64, u64, FrameKind, u32, u64);

#[derive(Debug, Clone, PartialEq)]
struct Outcome {
    frames: Vec<Seen>,
    end: ScanEnd,
    offset: u64,
    next_frame_seq: Option<u64>,
}

fn slice_scan(bytes: &[u8], base: u64, mode: SeqMode) -> Outcome {
    let s = FrameScanner::new(bytes, base);
    let mut s = match mode {
        SeqMode::Off => s,
        SeqMode::Contiguous => s.contiguous(),
        SeqMode::Expect(first) => s.expect_frame_seq(first),
    };
    let frames = s
        .by_ref()
        .map(|f| {
            let flags = u32::from(f.is_compressed());
            (f.offset(), f.total_len(), f.kind(), flags, f.frame_seq())
        })
        .collect();
    Outcome {
        frames,
        end: s.end().unwrap().clone(),
        offset: s.offset(),
        next_frame_seq: s.next_frame_seq(),
    }
}

fn stream_scanner(base: u64, len: usize, mode: SeqMode, chunk: usize) -> StreamScanner {
    let s = StreamScanner::new(base, base + len as u64);
    let s = match mode {
        SeqMode::Off => s,
        SeqMode::Contiguous => s.contiguous(),
        SeqMode::Expect(first) => s.expect_frame_seq(first),
    };
    s.with_chunk_len(chunk)
}

/// A reader over `bytes` at `base` that enforces invariant 3 and fails (once) at read number `fail_at`.
fn reader<'a>(
    bytes: &'a [u8],
    base: u64,
    chunk: usize,
    reads: &'a mut u64,
    fail_at: Option<u64>,
) -> impl FnMut(u64, &mut [u8]) -> Result<(), String> + 'a {
    move |offset: u64, buf: &mut [u8]| {
        let n = *reads;
        *reads += 1;
        if fail_at == Some(n) {
            return Err(format!("injected failure at read {n}"));
        }
        if buf.is_empty() || buf.len() > chunk.max(FRAME_HEADER_LEN) {
            return Err(format!("read of {} bytes, chunk {chunk}", buf.len()));
        }
        let start = offset
            .checked_sub(base)
            .ok_or_else(|| format!("read at {offset} below base {base}"))?;
        let start = usize::try_from(start).map_err(|e| e.to_string())?;
        let src = bytes
            .get(start..start + buf.len())
            .ok_or_else(|| format!("read {start}+{} past the range", buf.len()))?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

fn stream_scan(
    bytes: &[u8],
    base: u64,
    mode: SeqMode,
    chunk: usize,
) -> Result<Outcome, TestCaseError> {
    let mut s = stream_scanner(base, bytes.len(), mode, chunk);
    let mut reads = 0;
    let mut read = reader(bytes, base, chunk, &mut reads, None);
    let mut frames = Vec::new();
    while let Some(h) = s.next_frame(&mut read).map_err(TestCaseError::fail)? {
        frames.push((
            h.offset(),
            h.total_len(),
            h.kind(),
            h.flags(),
            h.frame_seq(),
        ));
    }
    prop_assert!(s.next_frame(&mut read).unwrap().is_none(), "fused");
    Ok(Outcome {
        frames,
        end: s.end().unwrap().clone(),
        offset: s.offset(),
        next_frame_seq: s.next_frame_seq(),
    })
}

// ---------------------------------------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FrameSpec {
    kind: u8,
    payload_len: usize,
    fill: u8,
    zstd: bool,
    /// `frame_seq` deviates from the run (a stale or skipped value).
    seq_jump: Option<u64>,
}

fn frame_spec() -> impl Strategy<Value = FrameSpec> {
    (
        prop_oneof![4 => 1_u8..=5, 1 => any::<u8>()],
        // Mostly larger than every chunk of `1..=40`, with every padding width.
        prop_oneof![1 => 0_usize..16, 4 => 40_usize..400, 1 => 1000_usize..3000],
        any::<u8>(),
        prop::bool::weighted(0.15),
        prop::option::weighted(0.1, prop_oneof![0_u64..4, Just(u64::MAX), any::<u64>()]),
    )
        .prop_map(|(kind, payload_len, fill, zstd, seq_jump)| FrameSpec {
            kind,
            payload_len,
            fill,
            zstd,
            seq_jump,
        })
}

/// A payload that is not all one byte, so damage to it is visible and zstd still shrinks it sometimes.
fn payload_of(spec: &FrameSpec) -> Vec<u8> {
    (0..spec.payload_len)
        .map(|i| spec.fill.wrapping_add((i % 7) as u8))
        .collect()
}

/// Encodes the frames with `frame_seq` = `first`, `first + 1`, ... (wrapping), returns the log and frame bounds.
fn encode_log(specs: &[FrameSpec], first: u64) -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut w = Writer::new();
    let mut bounds = Vec::new();
    for (i, s) in specs.iter().enumerate() {
        let start = w.len();
        let seq = s.seq_jump.unwrap_or_else(|| first.wrapping_add(i as u64));
        let compression = if s.zstd {
            Compression::Zstd { level: 1 }
        } else {
            Compression::None
        };
        encode_frame(
            &mut w,
            FrameKind::from_u8(s.kind),
            seq,
            &payload_of(s),
            compression,
        )
        .unwrap();
        bounds.push((start, w.len()));
    }
    (w.into_inner(), bounds)
}

/// Damage targeted at the parts of a frame whose bytes `StreamScanner` checks across chunk boundaries.
#[derive(Debug, Clone)]
enum Damage {
    /// A non-zero padding byte (or, with no padding, payload byte), CRC fixed up when the flag is set.
    Padding(Index, Index, u8, bool),
    /// A non-zero `reserved` byte (not covered by the CRC).
    Reserved(Index, Index, u8),
    CrcBit(Index, u8),
    PayloadBit(Index, Index, u8),
    /// Unknown `flags` bits with the CRC fixed up: an unsupported frame.
    UnknownFlags(Index, u32),
    /// A new `frame_seq` with the CRC fixed up: a valid frame that may break the run.
    Seq(Index, u64),
    /// A 512-byte sector lost (zeroed) anywhere.
    LostSector(Index),
    Truncate(Index),
    AppendZeros(usize),
    AppendBytes(Vec<u8>),
}

fn damage() -> impl Strategy<Value = Damage> {
    let i = any::<Index>;
    prop_oneof![
        3 => (i(), i(), 1_u8.., any::<bool>()).prop_map(|(f, a, v, fix)| Damage::Padding(f, a, v, fix)),
        2 => (i(), i(), 1_u8..).prop_map(|(f, a, v)| Damage::Reserved(f, a, v)),
        2 => (i(), 0_u8..32).prop_map(|(f, b)| Damage::CrcBit(f, b)),
        2 => (i(), i(), 0_u8..8).prop_map(|(f, a, b)| Damage::PayloadBit(f, a, b)),
        1 => (i(), 2_u32..1 << 24).prop_map(|(f, v)| Damage::UnknownFlags(f, v)),
        2 => (i(), prop_oneof![0_u64..8, Just(u64::MAX), any::<u64>()]).prop_map(|(f, v)| Damage::Seq(f, v)),
        1 => i().prop_map(Damage::LostSector),
        2 => i().prop_map(Damage::Truncate),
        1 => (1_usize..100).prop_map(Damage::AppendZeros),
        1 => proptest::collection::vec(any::<u8>(), 1..100).prop_map(Damage::AppendBytes),
    ]
}

fn fix_crc(b: &mut [u8], start: usize, end: usize) {
    let crc = crc32c(&b[start + 4..end - 8]);
    b[end - 8..end - 4].copy_from_slice(&crc.to_le_bytes());
}

fn apply(b: &mut Vec<u8>, bounds: &[(usize, usize)], d: &Damage) {
    let frame = |f: Index| -> Option<(usize, usize)> {
        let (s, e) = *bounds.get(f.index(bounds.len().max(1)))?;
        (e <= b.len()).then_some((s, e))
    };
    match *d {
        Damage::Padding(f, at, v, fix) => {
            if let Some((s, e)) = frame(f) {
                let payload_len = u64::from_le_bytes(b[s + 8..s + 16].try_into().unwrap()) as usize;
                let lo = (s + 24 + payload_len).min(e - 8);
                let pos = if e - 8 > lo {
                    lo + at.index(e - 8 - lo)
                } else if e - 8 > s + 24 {
                    s + 24 + at.index(e - 8 - s - 24)
                } else {
                    return;
                };
                b[pos] = v;
                if fix {
                    fix_crc(b, s, e);
                }
            }
        }
        Damage::Reserved(f, at, v) => {
            if let Some((_, e)) = frame(f) {
                b[e - 4 + at.index(4)] = v;
            }
        }
        Damage::CrcBit(f, bit) => {
            if let Some((_, e)) = frame(f) {
                b[e - 8 + usize::from(bit / 8)] ^= 1 << (bit % 8);
            }
        }
        Damage::PayloadBit(f, at, bit) => {
            if let Some((s, e)) = frame(f)
                && e - 8 > s + 24
            {
                b[s + 24 + at.index(e - 8 - s - 24)] ^= 1 << bit;
            }
        }
        Damage::UnknownFlags(f, flags) => {
            if let Some((s, e)) = frame(f) {
                let old = u32::from_le_bytes([b[s + 5], b[s + 6], b[s + 7], 0]);
                let new = (old & 1) | (flags & !1);
                b[s + 5..s + 8].copy_from_slice(&new.to_le_bytes()[..3]);
                fix_crc(b, s, e);
            }
        }
        Damage::Seq(f, seq) => {
            if let Some((s, e)) = frame(f) {
                b[s + 16..s + 24].copy_from_slice(&seq.to_le_bytes());
                fix_crc(b, s, e);
            }
        }
        Damage::LostSector(at) => {
            let from = at.index(b.len() + 1);
            let to = (from + 512).min(b.len());
            b[from..to].fill(0);
        }
        Damage::Truncate(at) => b.truncate(at.index(b.len() + 1)),
        Damage::AppendZeros(n) => b.resize(b.len() + n, 0),
        Damage::AppendBytes(ref tail) => b.extend_from_slice(tail),
    }
}

fn seq_mode(first: u64) -> impl Strategy<Value = SeqMode> {
    prop_oneof![
        1 => Just(SeqMode::Off),
        1 => Just(SeqMode::Contiguous),
        // Mostly the run's real start, so long runs pass and the damage decides where the scan stops.
        3 => Just(SeqMode::Expect(first)),
        1 => prop_oneof![Just(first.wrapping_add(1)), Just(first.wrapping_sub(1)), any::<u64>()]
            .prop_map(SeqMode::Expect),
    ]
}

fn first_seq() -> impl Strategy<Value = u64> {
    prop_oneof![
        3 => 1_u64..4,
        1 => Just(0_u64),
        // Runs that reach and pass `u64::MAX`.
        2 => (0_u64..6).prop_map(|k| u64::MAX - k),
    ]
}

/// A log of frames, damaged, plus the `frame_seq` its run starts at.
fn damaged_log(max_frames: usize) -> impl Strategy<Value = (Vec<u8>, u64)> {
    damaged_log_with_starts(max_frames).prop_map(|(bytes, first, _)| (bytes, first))
}

/// [`damaged_log`] plus where each frame started before the damage (relative to the log), which are still the
/// offsets a caller would check a frame at; a truncation may put some past the end.
fn damaged_log_with_starts(max_frames: usize) -> impl Strategy<Value = (Vec<u8>, u64, Vec<u64>)> {
    (
        proptest::collection::vec(frame_spec(), 0..max_frames),
        first_seq(),
        proptest::collection::vec(damage(), 0..3),
    )
        .prop_map(|(specs, first, damages)| {
            let (mut bytes, bounds) = encode_log(&specs, first);
            for d in &damages {
                apply(&mut bytes, &bounds, d);
            }
            let starts = bounds.iter().map(|&(s, _)| s as u64).collect();
            (bytes, first, starts)
        })
}

/// An 8-aligned base, small or such that the log ends within a few bytes of `u64::MAX`.
fn base_for(len: usize) -> impl Strategy<Value = u64> {
    let top = u64::MAX - len as u64;
    prop_oneof![
        Just(12288_u64),
        (0_u64..1 << 37).prop_map(|b| b * 8),
        // The end of the range is `u64::MAX - slack`, slack in 0..8 once aligned down.
        (0_u64..16).prop_map(move |slack| (top - slack) & !7),
    ]
}

fn case(max_frames: usize) -> impl Strategy<Value = (Vec<u8>, u64, u64, SeqMode)> {
    damaged_log(max_frames).prop_flat_map(|(bytes, first)| {
        let len = bytes.len();
        (Just(bytes), base_for(len), Just(first), seq_mode(first))
    })
}

/// Invariants 1-3.
fn check_all_chunkings(bytes: &[u8], base: u64, mode: SeqMode) -> Result<(), TestCaseError> {
    let want = slice_scan(bytes, base, mode);
    for chunk in chunk_lens() {
        let got = stream_scan(bytes, base, mode, chunk)?;
        prop_assert_eq!(&got, &want, "chunk {}", chunk);
    }
    Ok(())
}

/// Invariant 4: a read failure at read `fail_at`, then a retry with a good reader.
fn check_retry(
    bytes: &[u8],
    base: u64,
    mode: SeqMode,
    chunk: usize,
    fail_at: u64,
) -> Result<(), TestCaseError> {
    let want = stream_scan(bytes, base, mode, chunk)?;
    let mut s = stream_scanner(base, bytes.len(), mode, chunk);
    let mut frames = Vec::new();
    let mut reads = 0;
    let mut failed = false;
    loop {
        let before = (s.offset(), s.end().cloned(), s.next_frame_seq());
        let mut read = reader(bytes, base, chunk, &mut reads, (!failed).then_some(fail_at));
        match s.next_frame(&mut read) {
            Ok(Some(h)) => {
                frames.push((
                    h.offset(),
                    h.total_len(),
                    h.kind(),
                    h.flags(),
                    h.frame_seq(),
                ));
            }
            Ok(None) => break,
            Err(e) => {
                prop_assert!(!failed, "a second failure: {}", e);
                prop_assert!(e.starts_with("injected"), "{}", e);
                failed = true;
                prop_assert_eq!(
                    (s.offset(), s.end().cloned(), s.next_frame_seq()),
                    before,
                    "a failed read changed the scanner"
                );
            }
        }
    }
    let got = Outcome {
        frames,
        end: s.end().unwrap().clone(),
        offset: s.offset(),
        next_frame_seq: s.next_frame_seq(),
    };
    prop_assert_eq!(got, want);
    Ok(())
}

proptest! {
    #![proptest_config(config(128))]

    #[test]
    fn stream_scan_matches_slice_scan_under_every_chunking((bytes, base, _first, mode) in case(5)) {
        check_all_chunkings(&bytes, base, mode)?;
    }

    #[test]
    fn a_failed_read_is_retried_without_effect(
        (bytes, base, _first, mode) in case(4),
        chunk in prop_oneof![1_usize..41, Just(4096)],
        fail_at in prop_oneof![0_u64..4, 0_u64..400],
    ) {
        check_retry(&bytes, base, mode, chunk, fail_at)?;
    }
}

/// Bases at the very top of the address space, with a run that reaches `u64::MAX`, each check mode.
#[test]
fn frames_ending_at_u64_max_scan_alike() {
    let specs: Vec<FrameSpec> = [0_usize, 7, 40, 41, 300]
        .iter()
        .map(|&payload_len| FrameSpec {
            kind: 3,
            payload_len,
            fill: 9,
            zstd: false,
            seq_jump: None,
        })
        .collect();
    for first in [1, u64::MAX - 5, u64::MAX - 3, u64::MAX - 1] {
        let (bytes, _) = encode_log(&specs, first);
        for tail in [0_usize, 5, 8, 40] {
            let mut bytes = bytes.clone();
            bytes.resize(bytes.len() + tail, 0);
            let top = (u64::MAX - bytes.len() as u64) & !7;
            for base in [top, top - 8] {
                for mode in [
                    SeqMode::Off,
                    SeqMode::Contiguous,
                    SeqMode::Expect(first),
                    SeqMode::Expect(u64::MAX),
                ] {
                    check_all_chunkings(&bytes, base, mode).unwrap();
                }
            }
        }
    }
}

#[cfg(feature = "slow")]
proptest! {
    #![proptest_config(config(2000))]

    #[test]
    fn slow_stream_scan_matches_slice_scan_under_every_chunking((bytes, base, _first, mode) in case(10)) {
        check_all_chunkings(&bytes, base, mode)?;
    }
}

// ---------------------------------------------------------------------------------------------------------
// `StreamScanner::reframe` (ROADMAP 2.4.17): invariants 5-9
// ---------------------------------------------------------------------------------------------------------

/// A call `reframe` made to one of its closures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Read { offset: u64, len: usize },
    Write { len: usize },
}

#[derive(Debug, Clone, PartialEq)]
struct Reframed {
    result: Result<Result<FrameHeader, Error>, String>,
    written: Vec<u8>,
    ops: Vec<Op>,
}

/// `reframe` of the frame at absolute offset `o` of the log `bytes` at `base`, with closures that record every
/// call; call number `fail_at` (reads and writes counted together) fails with an injected error.
fn reframe_at(
    bytes: &[u8],
    base: u64,
    o: u64,
    chunk: usize,
    seq: u64,
    fail_at: Option<usize>,
) -> Reframed {
    let end = base + bytes.len() as u64;
    let ops = RefCell::new(Vec::new());
    let mut written = Vec::new();
    let injected = |ops: &Vec<Op>| {
        let n = ops.len() - 1;
        (fail_at == Some(n)).then(|| format!("injected failure at call {n}"))
    };
    let mut read = |offset: u64, buf: &mut [u8]| -> Result<(), String> {
        let mut ops = ops.borrow_mut();
        ops.push(Op::Read {
            offset,
            len: buf.len(),
        });
        if let Some(e) = injected(&ops) {
            return Err(e);
        }
        let start = offset
            .checked_sub(base)
            .ok_or_else(|| format!("read at {offset} below base {base}"))?;
        let start = usize::try_from(start).map_err(|e| e.to_string())?;
        let src = bytes
            .get(start..start + buf.len())
            .ok_or_else(|| format!("read {start}+{} past the range", buf.len()))?;
        buf.copy_from_slice(src);
        Ok(())
    };
    let mut write = |chunk: &[u8]| -> Result<(), String> {
        let mut ops = ops.borrow_mut();
        ops.push(Op::Write { len: chunk.len() });
        if let Some(e) = injected(&ops) {
            return Err(e);
        }
        written.extend_from_slice(chunk);
        Ok(())
    };
    let result = StreamScanner::new(o, end)
        .with_chunk_len(chunk)
        .reframe(seq, &mut read, &mut write);
    Reframed {
        result,
        written,
        ops: ops.into_inner(),
    }
}

/// `check_frame` at `o` with the same reads.
fn check_frame_at(bytes: &[u8], base: u64, o: u64, chunk: usize) -> Result<FrameHeader, Error> {
    let end = base + bytes.len() as u64;
    let mut reads = 0;
    let mut read = reader(bytes, base, chunk, &mut reads, None);
    StreamScanner::new(o, end)
        .with_chunk_len(chunk)
        .check_frame(&mut read)
        .unwrap()
}

/// What `reframe` at `o` must do, computed from the raw bytes and `Frame::decode` alone.
struct Want {
    /// `Frame::decode` over the bytes from `o` to the end, as `Seen`.
    decoded: Result<Seen, Error>,
    /// On success, `Frame::reframe(.., seq)` of the decoded frame.
    copy: Option<Vec<u8>>,
    /// What is written when the frame turns out invalid: its header with the new `frame_seq` and its stored
    /// payload if the header fits, nothing otherwise.
    partial: Vec<u8>,
    /// How many bytes from `o` are read.
    extent: u64,
}

fn want(bytes: &[u8], base: u64, o: u64, seq: u64) -> Want {
    let rest = o
        .checked_sub(base)
        .and_then(|rel| bytes.get(usize::try_from(rel).ok()?..))
        .unwrap_or_default();
    let (decoded, copy) = match Frame::decode(&mut Reader::with_base_offset(rest, o)) {
        Ok(f) => {
            let mut w = Writer::new();
            f.reframe(&mut w, seq);
            let flags = u32::from(f.is_compressed()) * FLAG_ZSTD;
            let seen = (f.offset(), f.total_len(), f.kind(), flags, f.frame_seq());
            (Ok(seen), Some(w.into_inner()))
        }
        Err(e) => (Err(e), None),
    };
    let (extent, partial) = if rest.len() < MIN_FRAME_LEN {
        (0, Vec::new())
    } else if rest[..4] != FRAME_MAGIC {
        (FRAME_HEADER_LEN as u64, Vec::new())
    } else {
        let payload_len = u64::from_le_bytes(rest[8..16].try_into().unwrap());
        let total = 24 + u128::from(payload_len).div_ceil(8) * 8 + 8;
        if total > rest.len() as u128 {
            (FRAME_HEADER_LEN as u64, Vec::new())
        } else {
            let mut partial = rest[..16].to_vec();
            partial.extend_from_slice(&seq.to_le_bytes());
            partial.extend_from_slice(&rest[24..24 + payload_len as usize]);
            (total as u64, partial)
        }
    };
    Want {
        decoded,
        copy,
        partial,
        extent,
    }
}

/// Invariants 5-7 for the frame at `o`, under one chunk length.
fn check_reframe(
    bytes: &[u8],
    base: u64,
    o: u64,
    chunk: usize,
    seq: u64,
) -> Result<(), TestCaseError> {
    let ctx = format!("offset {o} (base {base}), chunk {chunk}, seq {seq}");
    let want = want(bytes, base, o, seq);
    let got = reframe_at(bytes, base, o, chunk, seq, None);
    let result = match &got.result {
        Ok(result) => result,
        Err(e) => return Err(TestCaseError::fail(format!("{ctx}: closure error {e}"))),
    };
    // 5: the same as `check_frame`, and as `Frame::decode`.
    prop_assert_eq!(result, &check_frame_at(bytes, base, o, chunk), "{}", &ctx);
    let seen = result
        .as_ref()
        .map(|h| {
            (
                h.offset(),
                h.total_len(),
                h.kind(),
                h.flags(),
                h.frame_seq(),
            )
        })
        .map_err(Clone::clone);
    prop_assert_eq!(&seen, &want.decoded, "{}", &ctx);

    // 6: the copy.
    match (result, &want.copy) {
        (Ok(header), Some(copy)) => {
            prop_assert_eq!(&got.written, copy, "{}: the copy", &ctx);
            prop_assert_eq!(got.written.len() as u64, header.total_len(), "{}", &ctx);
            prop_assert_eq!(
                &got.written[..want.partial.len()],
                &want.partial[..],
                "{}",
                &ctx
            );
        }
        (Err(_), None) => {
            prop_assert_eq!(
                &got.written,
                &want.partial,
                "{}: an invalid frame's prefix",
                &ctx
            );
        }
        (result, copy) => {
            return Err(TestCaseError::fail(format!(
                "{ctx}: Ok-ness differs from Frame::decode: {result:?} vs {copy:?}"
            )));
        }
    }

    // 7: reads cover the extent once, in order, bounded; writes are bounded.
    let bound = chunk.max(FRAME_HEADER_LEN);
    let mut at = o;
    for op in &got.ops {
        match *op {
            Op::Read { offset, len } => {
                prop_assert!(len > 0 && len <= bound, "{}: read of {} bytes", &ctx, len);
                prop_assert_eq!(offset, at, "{}: reads are contiguous from the offset", &ctx);
                at += len as u64;
            }
            Op::Write { len } => {
                prop_assert!(len > 0 && len <= bound, "{}: write of {} bytes", &ctx, len);
            }
        }
    }
    prop_assert_eq!(at - o, want.extent, "{}: bytes read", &ctx);
    Ok(())
}

/// Invariant 8: a failure at every call of an undisturbed `reframe` is returned as is and is the last call.
fn check_reframe_failures(
    bytes: &[u8],
    base: u64,
    o: u64,
    chunk: usize,
    seq: u64,
) -> Result<(), TestCaseError> {
    let clean = reframe_at(bytes, base, o, chunk, seq, None);
    prop_assert!(clean.result.is_ok(), "{:?}", clean.result);
    for n in 0..=clean.ops.len() {
        let got = reframe_at(bytes, base, o, chunk, seq, Some(n));
        if n == clean.ops.len() {
            prop_assert_eq!(&got, &clean, "no call {} to fail", n);
            continue;
        }
        let expect = format!("injected failure at call {n}");
        prop_assert_eq!(&got.result, &Err(expect), "call {} of {:?}", n, clean.ops);
        prop_assert_eq!(
            &got.ops[..],
            &clean.ops[..=n],
            "nothing is called after a failure"
        );
        prop_assert!(clean.written.starts_with(&got.written));
    }
    Ok(())
}

/// A new `frame_seq` for the copy.
fn new_seq() -> impl Strategy<Value = u64> {
    prop_oneof![1_u64..10, Just(0), Just(u64::MAX), any::<u64>()]
}

/// A damaged log at a base, the absolute offsets to reframe at, and the copy's `frame_seq`.
fn reframe_case(max_frames: usize) -> impl Strategy<Value = (Vec<u8>, u64, Vec<u64>, u64)> {
    (
        damaged_log_with_starts(max_frames),
        proptest::collection::vec(any::<Index>(), 0..3),
        new_seq(),
    )
        .prop_flat_map(|((bytes, _first, starts), extra, seq)| {
            let len = bytes.len();
            let mut rel = starts;
            rel.extend([len as u64, len as u64 + 8]);
            rel.extend(extra.iter().map(|i| i.index(len + 1) as u64));
            (Just(bytes), base_for(len), Just(rel), Just(seq))
        })
        .prop_map(|(bytes, base, rel, seq)| {
            let offsets = rel.iter().map(|&r| base.saturating_add(r)).collect();
            (bytes, base, offsets, seq)
        })
}

proptest! {
    #![proptest_config(config(128))]

    #[test]
    fn reframe_matches_check_frame_and_frame_reframe_under_every_chunking(
        (bytes, base, offsets, seq) in reframe_case(5),
    ) {
        for &o in &offsets {
            for chunk in chunk_lens() {
                check_reframe(&bytes, base, o, chunk, seq)?;
            }
        }
    }

    #[test]
    fn reframe_returns_a_failed_read_or_write_as_is(
        (bytes, base, offsets, seq) in reframe_case(4),
        chunk in prop_oneof![8_usize..41, Just(64), Just(4096)],
    ) {
        for &o in &offsets {
            check_reframe_failures(&bytes, base, o, chunk, seq)?;
        }
    }
}

/// Invariant 9, deterministic: every frame of a log ending at `u64::MAX - slack`, compressed ones included, and
/// a copy renumbered to `u64::MAX`.
#[test]
fn reframe_near_u64_max() {
    let specs: Vec<FrameSpec> = [
        (0_usize, false),
        (7, false),
        (41, true),
        (300, true),
        (300, false),
    ]
    .iter()
    .map(|&(payload_len, zstd)| FrameSpec {
        kind: 3,
        payload_len,
        fill: 9,
        zstd,
        seq_jump: None,
    })
    .collect();
    let (bytes, bounds) = encode_log(&specs, u64::MAX - 2);
    for tail in [0_usize, 5, 8, 40] {
        let mut bytes = bytes.clone();
        bytes.resize(bytes.len() + tail, 0);
        let top = (u64::MAX - bytes.len() as u64) & !7;
        for base in [top, top - 8] {
            let mut offsets: Vec<u64> = bounds.iter().map(|&(s, _)| base + s as u64).collect();
            offsets.extend([base + bytes.len() as u64, u64::MAX]);
            for o in offsets {
                for chunk in chunk_lens() {
                    for seq in [0, u64::MAX] {
                        check_reframe(&bytes, base, o, chunk, seq).unwrap();
                    }
                }
            }
        }
    }
}

/// A large frame (well past any chunk) copied piece by piece: the doc example's shape, compressed and not, at a
/// chunk that does not divide the payload.
#[test]
fn reframe_copies_a_large_frame_exactly() {
    let payload: Vec<u8> = (0..300_000_u32)
        .map(|i| (i % 251) as u8 ^ (i / 1000) as u8)
        .collect();
    for compression in [Compression::None, Compression::Zstd { level: 1 }] {
        let mut w = Writer::new();
        encode_frame(&mut w, FrameKind::Segment, 5, &payload, compression).unwrap();
        let bytes = w.into_inner();
        let frame = Frame::decode(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(frame.is_compressed(), compression != Compression::None);
        for chunk in [4093, 65536] {
            check_reframe(&bytes, 12288, 12288, chunk, 77).unwrap();
        }
        let got = reframe_at(&bytes, 12288, 12288, 4093, 77, None);
        let copy = Frame::decode(&mut Reader::new(&got.written)).unwrap();
        assert_eq!(copy.frame_seq(), 77);
        assert_eq!(copy.stored_payload(), frame.stored_payload());
        assert_eq!(copy.is_compressed(), frame.is_compressed());
        assert_eq!(&*copy.payload(payload.len()).unwrap(), &payload[..]);
    }
}

/// A frame no larger than the chunk is copied with one write, and a larger one with one write per chunk of its bytes
/// (rounded up, plus one where the header and trailer spill over): header, payload and trailer are coalesced
/// (ROADMAP 2.4.21).
#[test]
fn reframe_coalesces_its_writes() {
    for (payload_len, chunk) in [
        (0, 64),
        (1, 64),
        (100, 65536),
        (4000, 4096),
        (100_000, 4096),
        (300_000, 65536),
    ] {
        let mut w = Writer::new();
        encode_frame(
            &mut w,
            FrameKind::WalChunk,
            1,
            &vec![3; payload_len],
            Compression::None,
        )
        .unwrap();
        let bytes = w.into_inner();
        let got = reframe_at(&bytes, 12288, 12288, chunk, 2, None);
        assert!(got.result.as_ref().is_ok_and(Result::is_ok));
        assert_eq!(got.written.len(), bytes.len());
        let writes: Vec<usize> = got
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Write { len } => Some(*len),
                Op::Read { .. } => None,
            })
            .collect();
        let most = bytes.len().div_ceil(chunk) + 1;
        if bytes.len() <= chunk {
            assert_eq!(
                writes,
                [bytes.len()],
                "payload {payload_len}, chunk {chunk}"
            );
        } else {
            assert!(
                writes.len() <= most,
                "payload {payload_len}, chunk {chunk}: {writes:?}"
            );
        }
        assert!(writes.iter().all(|&n| n <= chunk), "{writes:?}");
    }
}

#[cfg(feature = "slow")]
proptest! {
    #![proptest_config(config(1000))]

    #[test]
    fn slow_reframe_matches_check_frame_and_frame_reframe_under_every_chunking(
        (bytes, base, offsets, seq) in reframe_case(10),
    ) {
        for &o in &offsets {
            for chunk in chunk_lens() {
                check_reframe(&bytes, base, o, chunk, seq)?;
            }
        }
    }
}

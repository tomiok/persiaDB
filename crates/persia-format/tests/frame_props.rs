//! Property tests for frames and the frame scanner (ROADMAP 1.7.2.2; SPEC §4.4; ADR-0009). Golden bytes, single
//! validation offsets, every-bit CRC coverage of one frame, and hand-picked scan ends are unit tests in
//! `src/frame.rs`.
//!
//! Invariants:
//! 1. Round trip: `encode_frame` → `Frame::decode` gives back kind, `frame_seq` and payload (through
//!    `Frame::payload` with a limit of exactly the payload length; one byte less is rejected); the frame is
//!    `frame_len(stored len)` bytes; a frame is compressed only when zstd was asked for and made it smaller;
//!    every frame of a log that starts 8-aligned starts 8-aligned.
//! 2. Short file (ROADMAP 1.7.2.2): for every cut of a random log, the scanner yields exactly the frames that fit
//!    whole in the prefix, and ends `Clean` iff the cut is on a frame boundary, else `TornTail` at the start of
//!    the first incomplete frame. Never `Corrupt`.
//! 3. Grown file whose data did not land (the ADR-0009 crash model): the log from a cut on is zeros, possibly with
//!    more zeros appended. The scanner yields the frames left intact and ends `TornTail` at the first damaged
//!    frame (or `Clean` if nothing changed), never `Corrupt`. Cuts strictly inside the magic or the low bytes of
//!    `payload_len` are excluded: there a crash leaves non-zero bytes the model says cannot be followed by zeros
//!    within one aligned field.
//! 4. Differential: on randomly mutated logs (bit flips, byte overwrites, field edits, truncation, zero fill,
//!    appended garbage; CRCs optionally re-fixed so the padding, `reserved` and `flags` checks are reached) the
//!    scanner yields the same frames and the same `ScanEnd` (variant, offset and error) as a naive reference
//!    written straight from SPEC §4.4 and ADR-0009.
//! 5. A single bit flip inside frame `i` of a valid log never yields a frame with different content: the scan
//!    yields frames `0..i` unchanged and stops at frame `i` with a truncating end (torn tail or corruption).
//! 6. Ranged access (ROADMAP 1.7.5; SPEC §4.4 "Streaming and ranged access"): at every byte position of a
//!    randomly mutated log (the edits of invariant 4), `FrameHeader::decode` agrees with `Frame::decode`:
//!    - a valid frame has a header with the same kind, `frame_seq`, compression bit, no unknown flags,
//!      `payload_len` = stored length and `total_len` = the frame's;
//!    - with at least `MIN_FRAME_LEN` bytes left, a failed header is the identical `Frame::decode` error (bad
//!      magic at the frame, or `UnexpectedEof { needed: u64::MAX }` at `payload_len` when the length overflows);
//!      a header whose `total_len` runs past the input is `UnexpectedEof { needed: total_len }` at `payload_len`;
//!      any other `Frame::decode` failure is behind the CRC (checksum, padding/`reserved`, unknown flags, whose
//!      bits the header reports but does not reject);
//!    - with fewer than `FRAME_HEADER_LEN` bytes both fail at the frame with `UnexpectedEof` and the same
//!      `available`, needing 24 and 32; with 24..32 bytes `Frame::decode` fails that way and the header is
//!      judged on magic and length alone;
//!    - on error neither reader moves; a header advances exactly `FRAME_HEADER_LEN` bytes.

// Test-only helpers index, cast and unwrap on purpose (see reader_writer_props.rs).
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::cast_possible_truncation
)]

use persia_format::block::Compression;
use persia_format::checksum::crc32c;
use persia_format::frame::{
    FLAG_ZSTD, FRAME_HEADER_LEN, FRAME_MAGIC, Frame, FrameHeader, FrameKind, FrameScanner,
    KNOWN_FRAME_FLAGS, MIN_FRAME_LEN, ScanEnd, encode_frame, frame_len,
};
use persia_format::{Corruption, Error, Reader, Unsupported, Writer};
use proptest::prelude::*;
use proptest::sample::Index;

// ---------------------------------------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------------------------------------

/// What one frame of a generated log holds.
#[derive(Debug, Clone)]
struct Spec {
    kind: u8,
    seq: u64,
    payload: Vec<u8>,
    compression: Compression,
}

fn payload(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // Incompressible noise, including the empty payload and every padding width.
        proptest::collection::vec(any::<u8>(), 0..max),
        // A short repeated unit: zstd shrinks it, so compressed frames are common.
        (proptest::collection::vec(any::<u8>(), 1..6), 0..max).prop_map(|(unit, n)| unit
            .iter()
            .copied()
            .cycle()
            .take(n)
            .collect()),
    ]
}

fn spec(max_payload: usize) -> impl Strategy<Value = Spec> {
    (
        // Assigned kinds mostly, any byte (unknown kinds, 0 included) sometimes.
        prop_oneof![3 => 1_u8..=5, 1 => any::<u8>()],
        prop_oneof![any::<u64>(), 0_u64..4],
        payload(max_payload),
        prop_oneof![
            Just(Compression::None),
            Just(Compression::Zstd { level: 1 }),
            Just(Compression::Zstd { level: 3 }),
        ],
    )
        .prop_map(|(kind, seq, payload, compression)| Spec {
            kind,
            seq,
            payload,
            compression,
        })
}

fn specs(max_frames: usize, max_payload: usize) -> impl Strategy<Value = Vec<Spec>> {
    proptest::collection::vec(spec(max_payload), 0..max_frames)
}

/// An 8-aligned absolute offset for the start of the log (e.g. `log_end`).
fn aligned_base() -> impl Strategy<Value = u64> {
    prop_oneof![Just(12288_u64), (0_u64..1 << 37).prop_map(|b| b * 8)]
}

/// Encodes `specs` back to back; returns the log and each frame's `[start, end)` within it.
fn encode_log(specs: &[Spec]) -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut w = Writer::new();
    let mut bounds = Vec::new();
    for s in specs {
        let start = w.len();
        encode_frame(
            &mut w,
            FrameKind::from_u8(s.kind),
            s.seq,
            &s.payload,
            s.compression,
        )
        .unwrap();
        bounds.push((start, w.len()));
    }
    (w.into_inner(), bounds)
}

fn scan(bytes: &[u8], base: u64) -> (Vec<Frame<'_>>, ScanEnd) {
    let mut scanner = FrameScanner::new(bytes, base);
    let frames: Vec<_> = scanner.by_ref().collect();
    assert!(scanner.next().is_none(), "the scanner is fused");
    let end = scanner.end().unwrap().clone();
    assert_eq!(scanner.offset(), end.offset());
    (frames, end)
}

/// Checks that a full scan of an unmodified log matches `specs`, and returns its frames for the prefix checks.
fn check_full_log<'a>(
    log: &'a [u8],
    bounds: &[(usize, usize)],
    specs: &[Spec],
    base: u64,
) -> Result<Vec<Frame<'a>>, TestCaseError> {
    let (frames, end) = scan(log, base);
    prop_assert_eq!(
        end,
        ScanEnd::Clean {
            offset: base + log.len() as u64
        }
    );
    prop_assert_eq!(frames.len(), specs.len());
    for ((f, s), &(start, end)) in frames.iter().zip(specs).zip(bounds) {
        prop_assert_eq!(f.offset(), base + start as u64);
        prop_assert_eq!(f.end(), base + end as u64);
        prop_assert_eq!(f.offset() % 8, 0, "frames start 8-aligned (SPEC §4.1)");
        prop_assert_eq!(f.kind(), FrameKind::from_u8(s.kind));
        prop_assert_eq!(f.frame_seq(), s.seq);
        prop_assert_eq!(&*f.payload(s.payload.len()).unwrap(), &s.payload[..]);
    }
    Ok(frames)
}

// ---------------------------------------------------------------------------------------------------------
// Naive reference: SPEC §4.4 validation and ADR-0009 scan end classes, written straight from the text.
// ---------------------------------------------------------------------------------------------------------

/// A frame as the reference reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RefFrame {
    offset: u64,
    kind: u8,
    compressed: bool,
    seq: u64,
    stored: Vec<u8>,
}

impl RefFrame {
    fn of(f: &Frame<'_>) -> Self {
        Self {
            offset: f.offset(),
            kind: f.kind().to_u8(),
            compressed: f.is_compressed(),
            seq: f.frame_seq(),
            stored: f.stored_payload().to_vec(),
        }
    }
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// `24 + pad8(payload_len) + 8`, in `u128` so it cannot overflow.
fn ref_total_len(payload_len: u64) -> u128 {
    24 + u128::from(payload_len).div_ceil(8) * 8 + 8
}

fn corrupt(offset: u64, reason: Corruption) -> Error {
    Error::Corrupt { offset, reason }
}

/// Scans `b` (at absolute `base`) the way SPEC §4.4 describes it.
fn reference_scan(b: &[u8], base: u64) -> (Vec<RefFrame>, ScanEnd) {
    let mut frames = Vec::new();
    let mut pos = 0_usize;
    loop {
        let o = base + pos as u64;
        let rest = &b[pos..];
        if rest.is_empty() {
            return (frames, ScanEnd::Clean { offset: o });
        }
        // An invalid frame is a torn tail iff nothing but zeros follows its end (`None`: no known end, so from
        // its start).
        let invalid = |error: Error, known_end: Option<usize>| {
            let after = known_end.map_or(rest, |n| &rest[n..]);
            if after.iter().all(|&x| x == 0) {
                ScanEnd::TornTail { offset: o, error }
            } else {
                ScanEnd::Corrupt { offset: o, error }
            }
        };
        let available = rest.len() as u64;
        // 1. Room for the smallest frame, and the magic.
        if rest.len() < 32 {
            let error = corrupt(
                o,
                Corruption::UnexpectedEof {
                    needed: 32,
                    available,
                },
            );
            return (frames, invalid(error, Some(rest.len())));
        }
        if rest[..4] != *b"PFR1" {
            return (frames, invalid(corrupt(o, Corruption::BadFrameMagic), None));
        }
        // 2. Total length fits in u64 and in the input.
        let payload_len = u64_at(rest, 8);
        let total = ref_total_len(payload_len);
        if total > u128::from(available) {
            let needed = u64::try_from(total).unwrap_or(u64::MAX);
            let error = corrupt(o + 8, Corruption::UnexpectedEof { needed, available });
            return (frames, invalid(error, Some(rest.len())));
        }
        let n = total as usize;
        let crc_at = n - 8;
        // 3. CRC over kind..padding.
        let stored = u32_at(rest, crc_at);
        let computed = crc32c(&rest[4..crc_at]);
        if stored != computed {
            let error = corrupt(
                o + crc_at as u64,
                Corruption::ChecksumMismatch { stored, computed },
            );
            return (frames, invalid(error, Some(n)));
        }
        // 4. Zero padding, then zero reserved.
        let payload_end = 24 + payload_len as usize;
        let zero_ranges = [payload_end..crc_at, crc_at + 4..n];
        for range in zero_ranges {
            let start = range.start;
            if let Some(i) = rest[range].iter().position(|&x| x != 0) {
                let value = rest[start + i];
                let error = corrupt(
                    o + (start + i) as u64,
                    Corruption::NonZeroReserved { value },
                );
                return (frames, invalid(error, Some(n)));
            }
        }
        // Valid; unknown flags make it unsupported.
        let flags = u32::from_le_bytes([rest[5], rest[6], rest[7], 0]);
        if flags & !1 != 0 {
            let error = Error::Unsupported {
                offset: o + 5,
                reason: Unsupported::FrameFlags { flags: flags & !1 },
            };
            return (frames, ScanEnd::Unsupported { offset: o, error });
        }
        frames.push(RefFrame {
            offset: o,
            kind: rest[4],
            compressed: flags & 1 != 0,
            seq: u64_at(rest, 16),
            stored: rest[24..payload_end].to_vec(),
        });
        pos += n;
    }
}

/// Re-fixes the CRC of every frame along the chain from the start of `b`, stopping at the first position without
/// a magic and a total length that fits: random edits then reach the checks behind the CRC.
fn fix_crcs_along_chain(b: &mut [u8]) {
    let mut pos = 0;
    while b.len() - pos >= MIN_FRAME_LEN && b[pos..pos + 4] == FRAME_MAGIC {
        let total = ref_total_len(u64_at(b, pos + 8));
        if total > (b.len() - pos) as u128 {
            return;
        }
        let end = pos + total as usize;
        let crc = crc32c(&b[pos + 4..end - 8]);
        b[end - 8..end - 4].copy_from_slice(&crc.to_le_bytes());
        pos = end;
    }
}

// ---------------------------------------------------------------------------------------------------------
// Edits for the differential test
// ---------------------------------------------------------------------------------------------------------

/// One edit to an encoded log. Frame-targeted edits pick a frame by index among the original frames and are
/// skipped when the log has none (or the field is out of range after earlier edits).
#[derive(Debug, Clone)]
enum Edit {
    FlipBit(Index, u8),
    SetByte(Index, u8),
    Truncate(Index),
    /// The file grew but the data from here on did not land.
    ZeroFrom(Index),
    Append(Vec<u8>),
    AppendZeros(usize),
    /// Overwrites the 24-bit `flags` (often with unknown bits).
    SetFlags(Index, u32),
    SetKind(Index, u8),
    SetPayloadLen(Index, u64),
    /// Sets a byte of the frame's padding (or the first payload byte if it has none).
    SetPadding(Index, Index, u8),
    SetReserved(Index, Index, u8),
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        3 => (any::<Index>(), 0_u8..8).prop_map(|(i, b)| Edit::FlipBit(i, b)),
        2 => (any::<Index>(), any::<u8>()).prop_map(|(i, v)| Edit::SetByte(i, v)),
        2 => any::<Index>().prop_map(Edit::Truncate),
        2 => any::<Index>().prop_map(Edit::ZeroFrom),
        1 => proptest::collection::vec(any::<u8>(), 1..48).prop_map(Edit::Append),
        1 => (1_usize..80).prop_map(Edit::AppendZeros),
        2 => (any::<Index>(), prop_oneof![0_u32..4, 0_u32..1 << 24]).prop_map(|(f, v)| Edit::SetFlags(f, v)),
        1 => (any::<Index>(), any::<u8>()).prop_map(|(f, v)| Edit::SetKind(f, v)),
        1 => (any::<Index>(), prop_oneof![0_u64..400, Just(u64::MAX), Just(u64::MAX - 7), any::<u64>()])
            .prop_map(|(f, v)| Edit::SetPayloadLen(f, v)),
        2 => (any::<Index>(), any::<Index>(), 1_u8..).prop_map(|(f, i, v)| Edit::SetPadding(f, i, v)),
        1 => (any::<Index>(), any::<Index>(), 1_u8..).prop_map(|(f, i, v)| Edit::SetReserved(f, i, v)),
    ]
}

/// `(start, end, payload_len)` of the original frame `pick` selects, if its bytes are all still there.
fn target(bytes: &[u8], bounds: &[(usize, usize)], pick: Index) -> Option<(usize, usize, usize)> {
    if bounds.is_empty() {
        return None;
    }
    let (start, end) = bounds[pick.index(bounds.len())];
    (end <= bytes.len()).then(|| {
        let payload_len = usize::try_from(u64_at(bytes, start + 8)).unwrap_or(usize::MAX);
        (start, end, payload_len)
    })
}

fn apply_edit(bytes: &mut Vec<u8>, bounds: &[(usize, usize)], edit: &Edit) {
    let len = bytes.len();
    match *edit {
        Edit::FlipBit(at, bit) if len > 0 => bytes[at.index(len)] ^= 1 << bit,
        Edit::SetByte(at, value) if len > 0 => bytes[at.index(len)] = value,
        Edit::Truncate(at) => bytes.truncate(at.index(len + 1)),
        Edit::ZeroFrom(at) => bytes[at.index(len + 1)..].fill(0),
        Edit::Append(ref tail) => bytes.extend_from_slice(tail),
        Edit::AppendZeros(n) => bytes.resize(len + n, 0),
        Edit::SetFlags(pick, flags) => {
            if let Some((start, ..)) = target(bytes, bounds, pick) {
                bytes[start + 5..start + 8].copy_from_slice(&flags.to_le_bytes()[..3]);
            }
        }
        Edit::SetKind(pick, kind) => {
            if let Some((start, ..)) = target(bytes, bounds, pick) {
                bytes[start + 4] = kind;
            }
        }
        Edit::SetPayloadLen(pick, payload_len) => {
            if let Some((start, ..)) = target(bytes, bounds, pick) {
                bytes[start + 8..start + 16].copy_from_slice(&payload_len.to_le_bytes());
            }
        }
        Edit::SetPadding(pick, at, value) => {
            if let Some((start, end, payload_len)) = target(bytes, bounds, pick) {
                // Padding is `24 + payload_len .. end - 8`; with none, hit the first payload byte instead.
                let crc_at = end - 8;
                let lo = (start + 24).saturating_add(payload_len).min(crc_at);
                let pos = if crc_at > lo {
                    lo + at.index(crc_at - lo)
                } else {
                    start + 24
                };
                if pos < crc_at {
                    bytes[pos] = value;
                }
            }
        }
        Edit::SetReserved(pick, at, value) => {
            if let Some((_, end, _)) = target(bytes, bounds, pick) {
                bytes[end - 4 + at.index(4)] = value;
            }
        }
        Edit::FlipBit(..) | Edit::SetByte(..) => {}
    }
}

// ---------------------------------------------------------------------------------------------------------
// Property bodies (shared by the fast and slow suites)
// ---------------------------------------------------------------------------------------------------------

fn check_round_trip(s: &Spec, base: u64) -> Result<(), TestCaseError> {
    let mut w = Writer::new();
    encode_frame(
        &mut w,
        FrameKind::from_u8(s.kind),
        s.seq,
        &s.payload,
        s.compression,
    )
    .unwrap();
    let bytes = w.into_inner();
    let mut r = Reader::with_base_offset(&bytes, base);
    let f = Frame::decode(&mut r).unwrap();
    prop_assert!(r.is_empty());
    prop_assert_eq!(f.offset(), base);
    prop_assert_eq!(f.kind(), FrameKind::from_u8(s.kind));
    prop_assert_eq!(f.frame_seq(), s.seq);
    let stored = f.stored_payload().len() as u64;
    prop_assert_eq!(bytes.len() as u64, frame_len(stored).unwrap());
    prop_assert_eq!(f.total_len(), bytes.len() as u64);
    prop_assert_eq!(bytes.len() % 8, 0);
    prop_assert_eq!(f.end(), base + bytes.len() as u64);
    // Compressed only when asked for and smaller (encode_frame docs); otherwise stored as is.
    if f.is_compressed() {
        let asked = matches!(s.compression, Compression::Zstd { .. });
        prop_assert!(asked, "compressed without being asked to");
        prop_assert!(f.stored_payload().len() < s.payload.len());
        prop_assert_eq!(u64_at(f.stored_payload(), 0), s.payload.len() as u64);
    } else {
        prop_assert_eq!(f.stored_payload(), &s.payload[..]);
    }
    prop_assert_eq!(&*f.payload(s.payload.len()).unwrap(), &s.payload[..]);
    if let Some(max) = s.payload.len().checked_sub(1) {
        let err = f.payload(max).unwrap_err();
        let too_large = matches!(
            err.corruption(),
            Some((_, Corruption::UncompressedTooLarge { .. }))
        );
        prop_assert!(too_large, "bomb limit {} not enforced: {:?}", max, err);
    }
    // Deterministic: the same frame encodes to the same bytes.
    let mut again = Writer::new();
    encode_frame(
        &mut again,
        FrameKind::from_u8(s.kind),
        s.seq,
        &s.payload,
        s.compression,
    )
    .unwrap();
    prop_assert_eq!(again.as_slice(), &bytes[..]);
    Ok(())
}

/// Invariant 2: every cut of the log.
fn check_every_cut(specs: &[Spec], base: u64) -> Result<(), TestCaseError> {
    let (log, bounds) = encode_log(specs);
    let full = check_full_log(&log, &bounds, specs, base)?;
    for cut in 0..=log.len() {
        let (frames, end) = scan(&log[..cut], base);
        let whole = bounds.iter().take_while(|&&(_, e)| e <= cut).count();
        prop_assert_eq!(&frames[..], &full[..whole], "cut {}", cut);
        let boundary = cut == 0 || bounds.iter().any(|&(_, e)| e == cut);
        if boundary {
            prop_assert_eq!(
                end,
                ScanEnd::Clean {
                    offset: base + cut as u64
                },
                "cut {}",
                cut
            );
        } else {
            let torn_at = base + bounds[whole].0 as u64;
            let torn = matches!(end, ScanEnd::TornTail { offset, .. } if offset == torn_at);
            prop_assert!(
                torn,
                "cut {}: {:?}, expected a torn tail at {}",
                cut,
                end,
                torn_at
            );
        }
    }
    Ok(())
}

/// Invariant 3: zeros from every allowed cut on, plus `grow` more zero bytes.
fn check_zero_filled_tails(specs: &[Spec], base: u64, grow: usize) -> Result<(), TestCaseError> {
    let (log, bounds) = encode_log(specs);
    let full = check_full_log(&log, &bounds, specs, base)?;
    // Not strictly inside the magic, nor inside the low bytes of `payload_len` (payloads here are < 2^16 bytes,
    // so only those can be non-zero): a zeroed suffix of those fields leaves non-zero bytes in a field that a
    // sector-atomic write cannot split.
    let allowed = |cut: usize| {
        bounds
            .iter()
            .all(|&(s, _)| !(s + 1..s + 4).contains(&cut) && !(s + 9..s + 10).contains(&cut))
    };
    for cut in (0..=log.len()).filter(|&c| allowed(c)) {
        let mut bytes = log.clone();
        bytes[cut..].fill(0);
        bytes.resize(log.len() + grow, 0);
        // A frame survives iff its bytes did not change; the first changed one is the torn tail.
        let intact = bounds
            .iter()
            .take_while(|&&(s, e)| bytes[s..e] == log[s..e])
            .count();
        let (frames, end) = scan(&bytes, base);
        prop_assert_eq!(&frames[..], &full[..intact], "cut {}", cut);
        if intact == bounds.len() && grow == 0 {
            prop_assert_eq!(
                end,
                ScanEnd::Clean {
                    offset: base + log.len() as u64
                },
                "cut {}",
                cut
            );
        } else {
            let torn_at = base + bounds.get(intact).map_or(log.len(), |b| b.0) as u64;
            let torn = matches!(end, ScanEnd::TornTail { offset, .. } if offset == torn_at);
            prop_assert!(
                torn,
                "cut {}: {:?}, expected a torn tail at {}",
                cut,
                end,
                torn_at
            );
        }
    }
    Ok(())
}

/// Invariant 4.
fn check_matches_reference(
    specs: &[Spec],
    edits: &[Edit],
    fix_crcs: bool,
    base: u64,
) -> Result<(), TestCaseError> {
    let (mut bytes, bounds) = encode_log(specs);
    for e in edits {
        apply_edit(&mut bytes, &bounds, e);
    }
    if fix_crcs {
        fix_crcs_along_chain(&mut bytes);
    }
    let (expected_frames, expected_end) = reference_scan(&bytes, base);
    let (frames, end) = scan(&bytes, base);
    let got: Vec<_> = frames.iter().map(RefFrame::of).collect();
    prop_assert_eq!(got, expected_frames);
    prop_assert_eq!(&end, &expected_end);
    // A yielded frame's payload decodes or fails as corruption (a bad zstd payload in a valid frame), never
    // anything else.
    for f in &frames {
        if let Err(err) = f.payload(1 << 16) {
            prop_assert!(err.is_corruption(), "{:?}", err);
        }
    }
    Ok(())
}

/// Invariant 5, for the bit `bit` of byte `at` of frame `k`.
fn check_bit_flip(
    log: &[u8],
    bounds: &[(usize, usize)],
    full: &[Frame<'_>],
    base: u64,
    k: usize,
    at: usize,
    bit: u8,
) -> Result<(), TestCaseError> {
    let mut bytes = log.to_vec();
    bytes[bounds[k].0 + at] ^= 1 << bit;
    let (frames, end) = scan(&bytes, base);
    prop_assert_eq!(
        &frames[..],
        &full[..k],
        "frame {} byte {} bit {}",
        k,
        at,
        bit
    );
    prop_assert_eq!(end.offset(), base + bounds[k].0 as u64);
    prop_assert!(
        end.truncates(),
        "frame {} byte {} bit {}: {:?}",
        k,
        at,
        bit,
        end
    );
    Ok(())
}

/// Invariant 6 for the frame starting at byte `pos` of `b` (absolute offset `base + pos`).
fn check_header_against_frame(b: &[u8], base: u64, pos: usize) -> Result<(), TestCaseError> {
    let rest = &b[pos..];
    let o = base + pos as u64;
    let available = rest.len() as u64;
    let eof = |offset, needed| corrupt(offset, Corruption::UnexpectedEof { needed, available });

    let mut hr = Reader::with_base_offset(rest, o);
    let header = FrameHeader::decode(&mut hr);
    let mut fr = Reader::with_base_offset(rest, o);
    let frame = Frame::decode(&mut fr);

    // The header against the bytes, straight from SPEC §4.4: 24 bytes, magic, a length that fits in u64.
    let expected_header = if rest.len() < FRAME_HEADER_LEN {
        Err(eof(o, 24))
    } else if rest[..4] != FRAME_MAGIC {
        Err(corrupt(o, Corruption::BadFrameMagic))
    } else if ref_total_len(u64_at(rest, 8)) > u128::from(u64::MAX) {
        Err(eof(o + 8, u64::MAX))
    } else {
        Ok((
            rest[4],
            u32::from_le_bytes([rest[5], rest[6], rest[7], 0]),
            u64_at(rest, 8),
            u64_at(rest, 16),
            ref_total_len(u64_at(rest, 8)) as u64,
        ))
    };
    let got_header = header
        .as_ref()
        .map(|h| {
            (
                h.kind().to_u8(),
                h.flags(),
                h.payload_len(),
                h.frame_seq(),
                h.total_len(),
            )
        })
        .map_err(Clone::clone);
    prop_assert_eq!(&got_header, &expected_header, "at {}", pos);
    let header_pos = if header.is_ok() { FRAME_HEADER_LEN } else { 0 };
    prop_assert_eq!(hr.position(), header_pos, "at {}", pos);
    if let Ok(h) = &header {
        prop_assert_eq!(hr.offset(), o + FRAME_HEADER_LEN as u64);
        prop_assert_eq!(h.flags() >> 24, 0, "flags is a 24-bit field");
        prop_assert_eq!(h.offset(), o);
        // The range reader's bound (SPEC §4.4 rule 2): with the input ending at `o + available`, it fails exactly
        // when the frame runs past the end, with the error `Frame::decode` gives once 32 bytes remain.
        let fits = h.check_fits(o + available);
        prop_assert_eq!(fits.is_ok(), h.total_len() <= available, "at {}", pos);
        if let (Err(e), true) = (&fits, rest.len() >= MIN_FRAME_LEN) {
            prop_assert_eq!(frame.as_ref().err(), Some(e), "at {}", pos);
        }
    }
    let frame_pos = frame.as_ref().map_or(0, |f| f.total_len() as usize);
    prop_assert_eq!(fr.position(), frame_pos, "at {}", pos);

    if rest.len() < MIN_FRAME_LEN {
        prop_assert_eq!(frame.as_ref().err(), Some(&eof(o, 32)), "at {}", pos);
        return Ok(());
    }
    match (&header, &frame) {
        (_, Ok(f)) => {
            let h = header.as_ref().unwrap();
            prop_assert_eq!(h.kind(), f.kind());
            prop_assert_eq!(h.frame_seq(), f.frame_seq());
            prop_assert_eq!(h.flags() & FLAG_ZSTD != 0, f.is_compressed());
            prop_assert_eq!(h.flags() & !KNOWN_FRAME_FLAGS, 0);
            prop_assert_eq!(h.payload_len(), f.stored_payload().len() as u64);
            prop_assert_eq!(h.total_len(), f.total_len());
        }
        (Err(he), Err(fe)) => prop_assert_eq!(he, fe, "at {}", pos),
        (Ok(_), Err(fe)) if fe.is_corruption() => {
            let h = header.as_ref().unwrap();
            if h.total_len() > available {
                prop_assert_eq!(fe, &eof(o + 8, h.total_len()), "at {}", pos);
            } else {
                prop_assert!(
                    matches!(
                        fe.corruption(),
                        Some((
                            _,
                            Corruption::ChecksumMismatch { .. }
                                | Corruption::NonZeroReserved { .. }
                        ))
                    ),
                    "at {}: {:?}",
                    pos,
                    fe
                );
            }
        }
        (Ok(h), Err(fe)) => prop_assert_eq!(
            fe,
            &Error::Unsupported {
                offset: o + 5,
                reason: Unsupported::FrameFlags {
                    flags: h.flags() & !KNOWN_FRAME_FLAGS
                },
            },
            "at {}",
            pos
        ),
    }
    Ok(())
}

/// Invariant 6 on a mutated log: every byte position, so frame starts, mid-frame bytes and the short tail.
fn check_headers_on_mutated_log(
    specs: &[Spec],
    edits: &[Edit],
    fix_crcs: bool,
    base: u64,
) -> Result<(), TestCaseError> {
    let (mut bytes, bounds) = encode_log(specs);
    for e in edits {
        apply_edit(&mut bytes, &bounds, e);
    }
    if fix_crcs {
        fix_crcs_along_chain(&mut bytes);
    }
    for pos in 0..=bytes.len() {
        check_header_against_frame(&bytes, base, pos)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Fast suite
// ---------------------------------------------------------------------------------------------------------

proptest! {
    #[test]
    fn frame_round_trips(s in spec(600), base in prop_oneof![aligned_base(), any::<u64>().prop_map(|b| b >> 1)]) {
        check_round_trip(&s, base)?;
    }

    #[test]
    fn differential_against_reference(
        specs in specs(6, 120),
        edits in proptest::collection::vec(edit(), 0..4),
        fix_crcs in any::<bool>(),
        base in aligned_base(),
    ) {
        check_matches_reference(&specs, &edits, fix_crcs, base)?;
    }

    #[test]
    fn bit_flip_never_yields_different_content(
        specs in specs(5, 120).prop_filter("a frame to flip", |s| !s.is_empty()),
        frame in any::<Index>(),
        at in any::<Index>(),
        bit in 0_u8..8,
        base in aligned_base(),
    ) {
        let (log, bounds) = encode_log(&specs);
        let full = check_full_log(&log, &bounds, &specs, base)?;
        let k = frame.index(bounds.len());
        let (start, end) = bounds[k];
        check_bit_flip(&log, &bounds, &full, base, k, at.index(end - start), bit)?;
    }

    #[test]
    fn header_agrees_with_frame_decode_on_mutated_logs(
        specs in specs(5, 100),
        edits in proptest::collection::vec(edit(), 0..4),
        fix_crcs in any::<bool>(),
        base in aligned_base(),
    ) {
        check_headers_on_mutated_log(&specs, &edits, fix_crcs, base)?;
    }
}

proptest! {
    // Each case scans every cut of the log, so fewer, smaller cases keep the default run fast.
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn every_truncation_yields_exactly_the_whole_frames(specs in specs(6, 160), base in aligned_base()) {
        check_every_cut(&specs, base)?;
    }

    #[test]
    fn zero_filled_tail_is_torn_never_corrupt(
        specs in specs(6, 160),
        grow in prop_oneof![Just(0_usize), 1_usize..100],
        base in aligned_base(),
    ) {
        check_zero_filled_tails(&specs, base, grow)?;
    }
}

/// A fixed log covering every kind class, both compressions and every padding width, so the cut properties see
/// all of them at least once regardless of the random cases.
#[test]
fn every_cut_of_a_mixed_log() {
    let specs: Vec<Spec> = (0..10_u8)
        .map(|i| Spec {
            kind: [1, 2, 3, 4, 5, 0, 200, 255, 1, 3][usize::from(i)],
            seq: u64::from(i) * 7,
            payload: if i % 3 == 0 {
                b"persia ".repeat(usize::from(i) * 8 + 16)
            } else {
                (0..=usize::from(i) * 9).map(|x| x as u8 ^ 0x5a).collect()
            },
            compression: if i % 2 == 0 {
                Compression::Zstd { level: 3 }
            } else {
                Compression::None
            },
        })
        .collect();
    let (log, _) = encode_log(&specs);
    let compressed = scan(&log, 0).0.iter().filter(|f| f.is_compressed()).count();
    assert!(compressed >= 2, "the fixture should hold compressed frames");
    check_every_cut(&specs, 12288).unwrap();
    for grow in [0, 8, 40] {
        check_zero_filled_tails(&specs, 12288, grow).unwrap();
    }
}

// ---------------------------------------------------------------------------------------------------------
// Slow variants (nightly): longer logs, more cases, and every bit of every frame.
// ---------------------------------------------------------------------------------------------------------

#[cfg(feature = "slow")]
proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    #[test]
    fn slow_every_truncation(specs in specs(12, 400), base in aligned_base()) {
        check_every_cut(&specs, base)?;
    }

    #[test]
    fn slow_zero_filled_tail(specs in specs(12, 400), grow in 0_usize..200, base in aligned_base()) {
        check_zero_filled_tails(&specs, base, grow)?;
    }

    #[test]
    fn slow_differential(
        specs in specs(10, 300),
        edits in proptest::collection::vec(edit(), 0..10),
        fix_crcs in any::<bool>(),
        base in aligned_base(),
    ) {
        check_matches_reference(&specs, &edits, fix_crcs, base)?;
    }

    #[test]
    fn slow_header_agrees_with_frame_decode(
        specs in specs(10, 300),
        edits in proptest::collection::vec(edit(), 0..10),
        fix_crcs in any::<bool>(),
        base in aligned_base(),
    ) {
        check_headers_on_mutated_log(&specs, &edits, fix_crcs, base)?;
    }

    /// Every bit of every frame of a small log.
    #[test]
    fn slow_every_bit_flip(specs in specs(4, 64), base in aligned_base()) {
        let (log, bounds) = encode_log(&specs);
        let full = check_full_log(&log, &bounds, &specs, base)?;
        for (k, &(start, end)) in bounds.iter().enumerate() {
            for at in 0..end - start {
                for bit in 0..8 {
                    check_bit_flip(&log, &bounds, &full, base, k, at, bit)?;
                }
            }
        }
    }
}

/// The committed fuzz seeds (`fuzz/corpus/frame/`, first byte = mode; bit 0 fixes CRCs along the chain) are what
/// their names say, so they keep covering each scan end if the layout changes.
#[test]
fn fuzz_seeds_match_their_names() {
    const BASE: u64 = 12288; // the fuzz target's base for mode bit 2 clear
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fuzz/corpus/frame/");
    let seed = |name: &str| {
        let input = std::fs::read(format!("{dir}{name}")).unwrap();
        assert_eq!(
            input[0] & 6,
            0,
            "{name}: seeds use the plain base and chain fixing only"
        );
        let mut data = input[1..].to_vec();
        if input[0] & 1 != 0 {
            fix_crcs_along_chain(&mut data);
        }
        data
    };
    let class = |name: &str| {
        let data = seed(name);
        let (frames, end) = scan(&data, BASE);
        let compressed = frames.iter().filter(|f| f.is_compressed()).count();
        let reason = end.error().and_then(Error::corruption).map(|(_, r)| r);
        (frames.len(), compressed, end, reason)
    };

    let (n, compressed, end, _) = class("valid_mixed");
    assert_eq!((n, compressed), (5, 1));
    assert!(matches!(end, ScanEnd::Clean { .. }), "{end:?}");

    let (n, _, end, _) = class("torn_tail");
    assert_eq!(n, 4);
    assert!(matches!(end, ScanEnd::TornTail { .. }), "{end:?}");

    let (n, _, end, _) = class("zero_tail");
    assert_eq!(n, 1);
    assert!(
        matches!(end, ScanEnd::TornTail { offset, .. } if offset == BASE + 48),
        "{end:?}"
    );

    let (n, _, end, reason) = class("corrupt_first");
    assert_eq!(n, 0);
    assert!(
        matches!(end, ScanEnd::Corrupt { offset: BASE, .. }),
        "{end:?}"
    );
    assert!(matches!(reason, Some(Corruption::ChecksumMismatch { .. })));

    let (n, _, end, _) = class("unknown_flags_fix_crc");
    assert_eq!(n, 1);
    assert!(
        matches!(end, ScanEnd::Unsupported { offset, .. } if offset == BASE + 48),
        "{end:?}"
    );

    let (n, _, end, reason) = class("nonzero_padding_fix_crc");
    assert_eq!(n, 0);
    assert!(
        matches!(end, ScanEnd::Corrupt { offset: BASE, .. }),
        "{end:?}"
    );
    assert_eq!(reason, Some(Corruption::NonZeroReserved { value: 7 }));

    let data = seed("bad_zstd_payload_fix_crc");
    let (frames, end) = scan(&data, BASE);
    assert!(matches!(end, ScanEnd::Clean { .. }), "{end:?}");
    assert!(frames.len() == 1 && frames[0].is_compressed());
    assert!(frames[0].payload(1024).unwrap_err().is_corruption());
}

/// A checked scan (ADR-0013) accepts the longest prefix of the unchecked scan's frames whose `frame_seq` values are
/// in `1..u64::MAX`, start at `first` (if given) and step by one (a mismatch is reported before a value out of range);
/// where it stops early it reports corruption at the
/// breaking frame, and otherwise it ends exactly like the unchecked scan.
fn check_seq_run(specs: &[Spec], first: Option<u64>, base: u64) -> Result<(), TestCaseError> {
    let (log, _) = encode_log(specs);
    let (frames, unchecked_end) = scan(&log, base);
    let scanner = FrameScanner::new(&log, base);
    let mut scanner = match first {
        Some(first) => scanner.expect_frame_seq(first),
        None => scanner.contiguous(),
    };
    let checked: Vec<Frame<'_>> = scanner.by_ref().collect();
    let end = scanner.end().unwrap().clone();
    prop_assert_eq!(scanner.offset(), end.offset());

    let mut expected = first;
    let mut accepted = 0;
    let mut stop = None;
    for f in &frames {
        let seq = f.frame_seq();
        if let Some(e) = expected.filter(|&e| e != seq) {
            stop = Some((
                f.offset(),
                Corruption::FrameSeqMismatch {
                    expected: e,
                    found: seq,
                },
            ));
            break;
        }
        if seq == 0 || seq == u64::MAX {
            stop = Some((f.offset(), Corruption::FrameSeqOutOfRange { found: seq }));
            break;
        }
        expected = Some(seq + 1);
        accepted += 1;
    }
    prop_assert_eq!(&checked[..], &frames[..accepted]);
    if let Some((offset, reason)) = stop {
        let error = corrupt(offset + 16, reason);
        prop_assert_eq!(end, ScanEnd::Corrupt { offset, error });
    } else {
        prop_assert_eq!(end, unchecked_end);
        prop_assert_eq!(scanner.next_frame_seq(), expected);
    }
    Ok(())
}

proptest! {
    #[test]
    fn checked_scan_accepts_the_longest_valid_run(
        mut specs in specs(6, 60),
        start in prop_oneof![0_u64..3, (u64::MAX - 3)..=u64::MAX, any::<u64>()],
        jumps in proptest::collection::vec(prop_oneof![6 => Just(None), 1 => any::<u64>().prop_map(Some)], 6),
        first in prop_oneof![Just(None), Just(Some(0)), any::<u64>().prop_map(Some), Just(Some(u64::MAX))],
        use_start in any::<bool>(),
        base in aligned_base(),
    ) {
        let mut seq = start;
        for (i, (s, jump)) in specs.iter_mut().zip(&jumps).enumerate() {
            if i > 0 {
                seq = seq.wrapping_add(1);
            }
            seq = jump.unwrap_or(seq);
            s.seq = seq;
        }
        let first = if use_start { Some(start) } else { first };
        check_seq_run(&specs, first, base)?;
    }
}

proptest! {
    /// `reframe` changes only `frame_seq` and the CRC: the copy decodes to the same kind, flags and stored payload,
    /// and has the original's length.
    #[test]
    fn reframe_keeps_everything_but_frame_seq(s in spec(300), seq in any::<u64>(), base in aligned_base()) {
        let (log, _) = encode_log(std::slice::from_ref(&s));
        let original = Frame::decode(&mut Reader::with_base_offset(&log, base)).unwrap();
        let mut w = Writer::new();
        original.reframe(&mut w, seq);
        let copy_bytes = w.into_inner();
        prop_assert_eq!(copy_bytes.len(), log.len());
        let copy = Frame::decode(&mut Reader::with_base_offset(&copy_bytes, base)).unwrap();
        prop_assert_eq!(copy.frame_seq(), seq);
        prop_assert_eq!(copy.kind(), original.kind());
        prop_assert_eq!(copy.is_compressed(), original.is_compressed());
        prop_assert_eq!(copy.stored_payload(), original.stored_payload());
        prop_assert_eq!(&copy_bytes[..16], &log[..16]);
        prop_assert_eq!(&copy_bytes[24..copy_bytes.len() - 8], &log[24..log.len() - 8]);
    }
}

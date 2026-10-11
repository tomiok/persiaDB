//! Corruption and robustness tests for block codecs (ROADMAP 1.4.1-1.4.3; SPEC §5.2 "Codecs", ADR-0005).
//!
//! Invariants:
//! 1. The codec byte mapping is exactly `0 = None`, `1 = Zstd`; every other byte is `UnknownCodec` at the
//!    caller's offset, and `to_u8`/`from_u8` round-trip.
//! 2. Bomb guard: `uncompressed_len > max_len` is `UncompressedTooLarge` for both codecs and any payload,
//!    before any decoding or allocation (the capacity of `out` does not change), including `u64::MAX`.
//! 3. A zstd payload is exactly one RFC 8878 frame that declares a content size equal to `uncompressed_len`:
//!    skippable frames (alone, before, after), legacy magics, concatenated frames, leading or trailing bytes,
//!    frames without a content size, frames declaring another size, frames whose blocks expand to another
//!    size, truncated frames, frames that need a dictionary, frames with a wrong content checksum, reserved
//!    header bits and reserved block types are all rejected. Hand-crafted frames (raw and RLE blocks, every
//!    content-size field width, single-segment or not) that are well-formed decode exactly.
//! 4. On every error `out` is unchanged and the error offset is the caller's offset; structural rejections
//!    happen before `out` grows.
//! 5. The per-thread zstd contexts do not leak state: interleaving levels, failed decodes and failed
//!    compressions gives the same bytes as a fresh one-shot zstd context, and decodes stay correct.
//! 6. Every single-bit flip of a valid zstd payload either fails with a codec error at the caller's offset
//!    (leaving `out` untouched) or succeeds with exactly `uncompressed_len` bytes appended.

// Test-only helpers index, cast and unwrap on purpose (see reader_writer_props.rs).
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::cast_possible_truncation
)]

mod common;

use common::corrupt;
use persia_format::compress::{Codec, DEFAULT_ZSTD_LEVEL, compress_block, decompress_block};
use persia_format::{Corruption, Error};
use proptest::prelude::*;
use zstd::zstd_safe::{self, CCtx, CParameter};

const MAGIC: [u8; 4] = 0xfd2f_b528_u32.to_le_bytes();
const PREFIX: [u8; 3] = [0xaa, 0xbb, 0xcc];
const OFFSETS: [u64; 3] = [0, 0x1234_5678, u64::MAX];

// ---------------------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------------------

fn zstd(level: i32, src: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    compress_block(Codec::Zstd, level, src, &mut out).unwrap();
    out
}

/// A frame from a fresh zstd context with explicit parameters (content size, checksum, dictionary id).
fn zstd_with(src: &[u8], params: &[CParameter]) -> Vec<u8> {
    let mut cctx = CCtx::create();
    for &p in params {
        cctx.set_parameter(p).unwrap();
    }
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(src.len()));
    cctx.compress2(&mut out, src).unwrap();
    out
}

/// Decodes into a vector that already holds `PREFIX`, checking the shared error contract: on error the offset
/// is the caller's, the reason is a codec error and `out` still holds exactly `PREFIX`; on success exactly
/// `len` bytes were appended. Returns the decoded bytes or the reason.
#[allow(clippy::panic)] // test helper: any other error is a test failure
fn decode(
    codec: Codec,
    src: &[u8],
    len: u64,
    max: usize,
    offset: u64,
) -> Result<Vec<u8>, Corruption> {
    let mut out = PREFIX.to_vec();
    match decompress_block(codec, src, len, max, offset, &mut out) {
        Ok(()) => {
            assert_eq!(out.len() as u64, PREFIX.len() as u64 + len, "{src:x?}");
            assert_eq!(out[..PREFIX.len()], PREFIX);
            Ok(out.split_off(PREFIX.len()))
        }
        Err(Error::Corrupt { offset: at, reason }) => {
            assert_eq!(at, offset, "error offset must be the caller's offset");
            assert!(
                matches!(
                    reason,
                    Corruption::UncompressedTooLarge { .. }
                        | Corruption::UncompressedLenMismatch { .. }
                        | Corruption::InvalidCompressedData
                ),
                "{reason:?}"
            );
            assert_eq!(out, PREFIX, "a failed decode must leave `out` unchanged");
            Err(reason)
        }
        Err(other) => panic!("decompression returns only Error::Corrupt: {other:?}"),
    }
}

/// Like [`decode`], but also asserts that `out` never grew: the payload is rejected from its header alone,
/// before `len` bytes are allocated (ADR-0005, decision 2).
#[allow(clippy::panic)] // test helper: any other error is a test failure
fn reject_early(src: &[u8], len: u64, max: usize) -> Corruption {
    for offset in OFFSETS {
        let mut out = Vec::with_capacity(PREFIX.len());
        out.extend_from_slice(&PREFIX);
        let cap = out.capacity();
        let err = decompress_block(Codec::Zstd, src, len, max, offset, &mut out).unwrap_err();
        assert_eq!(out, PREFIX);
        assert_eq!(out.capacity(), cap, "rejected before allocating: {src:x?}");
        let Error::Corrupt { offset: at, .. } = err else {
            panic!("{err:?}")
        };
        assert_eq!(at, offset);
    }
    let err = decompress_block(Codec::Zstd, src, len, max, 0, &mut Vec::new()).unwrap_err();
    let Error::Corrupt { reason, .. } = err else {
        panic!("{err:?}")
    };
    reason
}

// ---------------------------------------------------------------------------------------------------------
// A minimal RFC 8878 §3.1.1 frame builder, so tests do not depend on what zstd's encoder chooses to emit.
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Fcs {
    /// No content size field (only valid without the single-segment flag).
    Absent,
    /// Field of 1 (single-segment only), 2, 4 or (any other width) 8 bytes holding `value`.
    Field(u8, u64),
}

#[derive(Debug, Clone)]
enum Block {
    Raw(Vec<u8>),
    Rle(u8, u32),
    /// `Block_Type` 3, which RFC 8878 §3.1.1.2.2 reserves.
    Reserved,
}

#[derive(Debug, Clone)]
struct Frame {
    single_segment: bool,
    fcs: Fcs,
    /// `Dictionary_ID` field width (0, 1, 2 or, for any other width, 4) and value.
    dict: (u8, u32),
    /// `Content_Checksum`: the 4 bytes to append, if the flag is set.
    checksum: Option<[u8; 4]>,
    /// Sets the reserved bit 3 of the frame header descriptor.
    reserved_bit: bool,
    blocks: Vec<Block>,
}

impl Frame {
    /// A well-formed single-segment frame with one raw block, declaring its true size.
    fn raw(content: &[u8]) -> Self {
        let n = content.len() as u64;
        Self {
            single_segment: true,
            fcs: Fcs::Field(fcs_width(n), n),
            dict: (0, 0),
            checksum: None,
            reserved_bit: false,
            blocks: vec![Block::Raw(content.to_vec())],
        }
    }

    fn content(&self) -> Vec<u8> {
        let mut v = Vec::new();
        for b in &self.blocks {
            match b {
                Block::Raw(bytes) => v.extend_from_slice(bytes),
                Block::Rle(byte, n) => v.extend(std::iter::repeat_n(*byte, *n as usize)),
                Block::Reserved => {}
            }
        }
        v
    }

    fn encode(&self) -> Vec<u8> {
        let (fcs_flag, fcs_bytes): (u8, Vec<u8>) = match self.fcs {
            Fcs::Absent => (0, vec![]),
            Fcs::Field(1, v) => (0, vec![v as u8]),
            Fcs::Field(2, v) => (1, ((v - 256) as u16).to_le_bytes().to_vec()),
            Fcs::Field(4, v) => (2, (v as u32).to_le_bytes().to_vec()),
            Fcs::Field(_, v) => (3, v.to_le_bytes().to_vec()),
        };
        let (dict_flag, dict_bytes): (u8, Vec<u8>) = match self.dict {
            (0, _) => (0, vec![]),
            (1, id) => (1, vec![id as u8]),
            (2, id) => (2, (id as u16).to_le_bytes().to_vec()),
            (_, id) => (3, id.to_le_bytes().to_vec()),
        };
        let descriptor = (fcs_flag << 6)
            | (u8::from(self.single_segment) << 5)
            | (u8::from(self.reserved_bit) << 3)
            | (u8::from(self.checksum.is_some()) << 2)
            | dict_flag;
        let mut v = MAGIC.to_vec();
        v.push(descriptor);
        if !self.single_segment {
            // Window_Descriptor: exponent 7, mantissa 0 = 128 KiB, enough for every block used here.
            v.push(7 << 3);
        }
        v.extend(dict_bytes);
        v.extend(fcs_bytes);
        let last = self.blocks.len().saturating_sub(1);
        for (i, b) in self.blocks.iter().enumerate() {
            let (kind, size, body): (u32, u32, Vec<u8>) = match b {
                Block::Raw(bytes) => (0, bytes.len() as u32, bytes.clone()),
                Block::Rle(byte, n) => (1, *n, vec![*byte]),
                Block::Reserved => (3, 1, vec![0]),
            };
            let header = u32::from(i == last) | (kind << 1) | (size << 3);
            v.extend_from_slice(&header.to_le_bytes()[..3]);
            v.extend(body);
        }
        if let Some(sum) = self.checksum {
            v.extend_from_slice(&sum);
        }
        v
    }
}

/// The smallest `Frame_Content_Size` field width for `n` in a single-segment frame.
fn fcs_width(n: u64) -> u8 {
    match n {
        0..=255 => 1,
        256..=65_791 => 2,
        _ if u32::try_from(n).is_ok() => 4,
        _ => 8,
    }
}

/// The correct `Content_Checksum` of `content` (low 32 bits of XXH64), taken from zstd's own encoder so the
/// test needs no xxh64 implementation.
fn checksum_of(content: &[u8]) -> [u8; 4] {
    let framed = zstd_with(content, &[CParameter::ChecksumFlag(true)]);
    framed[framed.len() - 4..].try_into().unwrap()
}

fn skippable(nibble: u8, body: &[u8]) -> Vec<u8> {
    let mut v = (0x184d_2a50_u32 | u32::from(nibble)).to_le_bytes().to_vec();
    v.extend_from_slice(&(body.len() as u32).to_le_bytes());
    v.extend_from_slice(body);
    v
}

fn text() -> Vec<u8> {
    b"persia persia persia, never zero results ".repeat(8)
}

// ---------------------------------------------------------------------------------------------------------
// 1. Codec byte mapping
// ---------------------------------------------------------------------------------------------------------

proptest! {
    #[test]
    fn codec_byte_mapping_is_exact(byte in any::<u8>(), offset in any::<u64>()) {
        let expected = match byte {
            0 => Ok(Codec::None),
            1 => Ok(Codec::Zstd),
            codec => Err(corrupt(offset, Corruption::UnknownCodec { codec })),
        };
        let parsed = Codec::from_u8(byte, offset);
        prop_assert_eq!(parsed.clone(), expected);
        if let Ok(codec) = parsed {
            prop_assert_eq!(codec.to_u8(), byte);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// 2. Bomb guard
// ---------------------------------------------------------------------------------------------------------

/// Payloads that would each be rejected (or accepted) differently if decoding ran: the guard must not care.
fn bomb_payloads() -> Vec<Vec<u8>> {
    let mut rle = Frame::raw(b"");
    rle.blocks = vec![Block::Rle(b'x', 100_000)];
    rle.fcs = Fcs::Field(4, 100_000);
    vec![
        vec![],
        b"junk".to_vec(),
        zstd(DEFAULT_ZSTD_LEVEL, &text()),
        rle.encode(),
        skippable(0, b"abc"),
        MAGIC.to_vec(),
    ]
}

/// The limit `decompress_block` reports: the caller's, capped at `isize::MAX` (no allocation is larger).
fn effective(max: usize) -> u64 {
    max.min(isize::MAX as usize) as u64
}

#[test]
fn bomb_guard_fires_first_for_every_codec_and_payload() {
    for codec in [Codec::None, Codec::Zstd] {
        for payload in bomb_payloads() {
            for (len, max) in [
                (1, 0),
                (100_000, 99_999),
                (u64::MAX, 1 << 20),
                (u64::MAX, usize::MAX - 1),
                (usize::MAX as u64, usize::MAX - 1),
            ] {
                for offset in OFFSETS {
                    let mut out = Vec::with_capacity(PREFIX.len());
                    out.extend_from_slice(&PREFIX);
                    let cap = out.capacity();
                    assert_eq!(
                        decompress_block(codec, &payload, len, max, offset, &mut out),
                        Err(corrupt(
                            offset,
                            Corruption::UncompressedTooLarge {
                                len,
                                max: effective(max)
                            }
                        )),
                        "{codec:?} {payload:x?}"
                    );
                    assert_eq!(out, PREFIX);
                    assert_eq!(out.capacity(), cap, "the guard must not allocate");
                }
            }
        }
    }
}

proptest! {
    #[test]
    fn bomb_guard_any_garbage(
        zstd_codec in any::<bool>(),
        payload in proptest::collection::vec(any::<u8>(), 0..64),
        max in prop_oneof![0_usize..1 << 16, Just(usize::MAX - 1)],
        excess in prop_oneof![1_u64..10, any::<u64>()],
        offset in any::<u64>(),
    ) {
        let codec = if zstd_codec { Codec::Zstd } else { Codec::None };
        let len = (max as u64).saturating_add(excess.max(1));
        prop_assume!(len > max as u64);
        let mut out = PREFIX.to_vec();
        prop_assert_eq!(
            decompress_block(codec, &payload, len, max, offset, &mut out),
            Err(corrupt(offset, Corruption::UncompressedTooLarge { len, max: effective(max) }))
        );
        prop_assert_eq!(out, PREFIX);
    }
}

/// With the guard opened all the way (`max_len == usize::MAX`), a frame that declares a length no `Vec` can
/// hold, and a header that agrees with it, must still be a clean error: library code never panics on input
/// from disk (CLAUDE.md "Safety & errors"). Above `isize::MAX` no allocation can ever succeed, so that is
/// corruption; at `isize::MAX` the allocation itself fails, which is a resource error (ROADMAP 1.4.4).
#[test]
fn impossible_declared_len_with_open_guard_is_an_error_not_a_panic() {
    let isize_max = isize::MAX as u64;
    // u64::MAX - 2 is the largest value zstd does not reserve as "unknown"/"error".
    for (declared, expected) in [
        (
            u64::MAX - 2,
            corrupt(
                9,
                Corruption::UncompressedTooLarge {
                    len: u64::MAX - 2,
                    max: isize_max,
                },
            ),
        ),
        (
            isize_max + 1,
            corrupt(
                9,
                Corruption::UncompressedTooLarge {
                    len: isize_max + 1,
                    max: isize_max,
                },
            ),
        ),
        (
            isize_max,
            Error::OutOfMemory {
                bytes: Some(isize_max),
            },
        ),
    ] {
        let mut frame = Frame::raw(b"");
        frame.single_segment = false;
        frame.fcs = Fcs::Field(8, declared);
        frame.blocks = vec![Block::Rle(0, 1)];
        let payload = frame.encode();
        let result = std::panic::catch_unwind(|| {
            let mut out = Vec::new();
            decompress_block(Codec::Zstd, &payload, declared, usize::MAX, 9, &mut out)
        });
        let result = result.expect("decompress_block panicked instead of returning an error");
        assert_eq!(result, Err(expected));
    }
}

// ---------------------------------------------------------------------------------------------------------
// 3. The one-frame rule (RFC 8878)
// ---------------------------------------------------------------------------------------------------------

#[test]
fn hand_crafted_well_formed_frames_decode() {
    let text = text();
    let big = vec![b'z'; 70_000];
    let mut frames = vec![Frame::raw(b""), Frame::raw(b"a"), Frame::raw(&text)];
    // Every content-size width; single-segment and windowed.
    for (width, single) in [
        (1, true),
        (2, true),
        (2, false),
        (4, false),
        (8, false),
        (8, true),
    ] {
        let content: &[u8] = if width == 1 { b"abc" } else { &text };
        let mut f = Frame::raw(content);
        let n = content.len() as u64;
        f.fcs = Fcs::Field(width, if width == 2 { n.max(256) } else { n });
        if width == 2 && n < 256 {
            continue;
        }
        f.single_segment = single;
        frames.push(f);
    }
    // Multi-block: raw then RLE, crossing the 2-byte FCS boundary.
    let mut multi = Frame::raw(&text);
    multi.blocks.push(Block::Rle(b'!', 1000));
    multi.single_segment = false;
    let n = multi.content().len() as u64;
    multi.fcs = Fcs::Field(2, n);
    frames.push(multi);
    // RLE larger than any raw input, and a correct content checksum.
    let mut rle = Frame::raw(b"");
    rle.blocks = vec![Block::Rle(b'z', 70_000)];
    rle.fcs = Fcs::Field(4, 70_000);
    frames.push(rle);
    let mut checked = Frame::raw(&text);
    checked.checksum = Some(checksum_of(&text));
    frames.push(checked);
    // An explicit zero dictionary id means "no dictionary".
    let mut dict_zero = Frame::raw(&text);
    dict_zero.dict = (4, 0);
    frames.push(dict_zero);

    for f in &frames {
        let payload = f.encode();
        let content = f.content();
        let len = content.len() as u64;
        for offset in OFFSETS {
            assert_eq!(
                decode(Codec::Zstd, &payload, len, content.len(), offset).as_deref(),
                Ok(&content[..]),
                "{f:?}"
            );
        }
    }
    assert_eq!(frames[frames.len() - 3].content(), big);
}

#[test]
fn skippable_frames_are_rejected_anywhere() {
    let text = text();
    let frame = zstd(DEFAULT_ZSTD_LEVEL, &text);
    let len = text.len() as u64;
    for nibble in [0x0, 0x7, 0xf] {
        for body in [&b""[..], b"x", &text] {
            let skip = skippable(nibble, body);
            let before = [skip.clone(), frame.clone()].concat();
            let after = [frame.clone(), skip.clone()].concat();
            let around = [skip.clone(), frame.clone(), skip.clone()].concat();
            for (name, payload) in [
                ("alone", &skip),
                ("before", &before),
                ("after", &after),
                ("around", &around),
            ] {
                assert_eq!(
                    reject_early(payload, len, 1 << 20),
                    Corruption::InvalidCompressedData,
                    "skippable {name}, nibble {nibble:x}, body {}",
                    body.len()
                );
            }
        }
    }
}

#[test]
fn other_magics_are_rejected() {
    let text = text();
    let frame = zstd(DEFAULT_ZSTD_LEVEL, &text);
    // Legacy zstd v0.1-v0.7 magics, the dictionary magic, and a near miss of each magic byte.
    let mut magics: Vec<[u8; 4]> = (0x1e_b5_2f_fd_u32..=0x27_b5_2f_fd)
        .step_by(0x0100_0000)
        .map(u32::to_be_bytes)
        .collect();
    magics.push(0xec30_a437_u32.to_le_bytes());
    for i in 0..4 {
        let mut m = MAGIC;
        m[i] ^= 1;
        magics.push(m);
    }
    for magic in magics {
        let mut payload = frame.clone();
        payload[..4].copy_from_slice(&magic);
        assert_eq!(
            reject_early(&payload, text.len() as u64, 1 << 20),
            Corruption::InvalidCompressedData,
            "{magic:x?}"
        );
    }
}

#[test]
fn concatenated_frames_and_extra_bytes_are_rejected() {
    let text = text();
    let (a, b) = text.split_at(100);
    let len = text.len() as u64;
    let frame = zstd(DEFAULT_ZSTD_LEVEL, &text);
    let empty = zstd(DEFAULT_ZSTD_LEVEL, b"");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        // Two halves whose sizes add up to `len`: only the first frame declares a size.
        ("split halves", [zstd(1, a), zstd(1, b)].concat()),
        ("then empty frame", [frame.clone(), empty.clone()].concat()),
        ("empty frame first", [empty.clone(), frame.clone()].concat()),
        ("leading zero", [vec![0], frame.clone()].concat()),
        ("leading magic", [MAGIC.to_vec(), frame.clone()].concat()),
        ("trailing magic", [frame.clone(), MAGIC.to_vec()].concat()),
        ("trailing zeros", [frame.clone(), vec![0; 3]].concat()),
    ];
    for (name, payload) in cases {
        let reason = reject_early(&payload, len, 1 << 20);
        assert_eq!(reason, Corruption::InvalidCompressedData, "{name}");
    }
}

#[test]
fn missing_or_wrong_declared_size_is_rejected_early() {
    let text = text();
    let len = text.len() as u64;
    // Windowed frame without a content size field.
    let mut absent = Frame::raw(&text);
    absent.single_segment = false;
    absent.fcs = Fcs::Absent;
    assert_eq!(
        reject_early(&absent.encode(), len, 1 << 20),
        Corruption::InvalidCompressedData
    );
    // zstd's own encoder with the content size flag off.
    let no_size = zstd_with(&text, &[CParameter::ContentSizeFlag(false)]);
    assert_eq!(
        reject_early(&no_size, len, 1 << 20),
        Corruption::InvalidCompressedData
    );

    // Declared sizes that disagree with the header, including ones far above the guard.
    for declared in [0, len - 1, len + 1, 1 << 40, u64::MAX - 2] {
        let mut f = Frame::raw(&text);
        f.single_segment = false;
        f.fcs = Fcs::Field(8, declared);
        assert_eq!(
            reject_early(&f.encode(), len, 1 << 20),
            Corruption::UncompressedLenMismatch {
                expected: len,
                actual: declared
            },
            "declared {declared}"
        );
    }
}

#[test]
fn blocks_that_disagree_with_the_declared_size_are_rejected() {
    let text = text();
    let len = text.len() as u64;
    for content_len in [0, text.len() - 1, text.len() + 1, 2 * text.len()] {
        let content = text
            .iter()
            .copied()
            .cycle()
            .take(content_len)
            .collect::<Vec<_>>();
        let mut f = Frame::raw(&content);
        f.single_segment = false;
        f.fcs = Fcs::Field(8, len);
        let reason = decode(Codec::Zstd, &f.encode(), len, 1 << 20, 5).unwrap_err();
        assert!(
            matches!(
                reason,
                Corruption::InvalidCompressedData | Corruption::UncompressedLenMismatch { .. }
            ),
            "{content_len}: {reason:?}"
        );
    }
}

#[test]
fn truncation_at_every_offset_is_rejected() {
    let text = text();
    let mut checked = Frame::raw(&text);
    checked.checksum = Some(checksum_of(&text));
    let mut multi = Frame::raw(&text);
    multi.blocks.push(Block::Rle(b'!', 10));
    multi.fcs = Fcs::Field(2, multi.content().len() as u64);
    let payloads = [
        (text.clone(), zstd(DEFAULT_ZSTD_LEVEL, &text)),
        (
            text.clone(),
            zstd_with(&text, &[CParameter::ChecksumFlag(true)]),
        ),
        (text.clone(), checked.encode()),
        (multi.content(), multi.encode()),
    ];
    for (content, payload) in payloads {
        for cut in 0..payload.len() {
            assert_eq!(
                reject_early(&payload[..cut], content.len() as u64, 1 << 20),
                Corruption::InvalidCompressedData,
                "cut at {cut} of {}",
                payload.len()
            );
        }
        assert_eq!(
            decode(Codec::Zstd, &payload, content.len() as u64, 1 << 20, 0),
            Ok(content)
        );
    }
}

#[test]
fn frames_that_need_a_dictionary_are_rejected() {
    let text = text();
    for (width, id) in [
        (1, 1),
        (1, 0xff),
        (2, 0x1234),
        (4, 0x8000_0000),
        (4, u32::MAX),
    ] {
        let mut f = Frame::raw(&text);
        f.dict = (width, id);
        assert_eq!(
            decode(Codec::Zstd, &f.encode(), text.len() as u64, 1 << 20, 3),
            Err(Corruption::InvalidCompressedData),
            "dictionary id {id:#x} ({width} bytes)"
        );
    }
}

#[test]
fn wrong_content_checksum_is_rejected() {
    let text = text();
    let good = checksum_of(&text);
    for i in 0..4 {
        for bit in 0..8 {
            let mut bad = good;
            bad[i] ^= 1 << bit;
            let mut f = Frame::raw(&text);
            f.checksum = Some(bad);
            assert_eq!(
                decode(Codec::Zstd, &f.encode(), text.len() as u64, 1 << 20, 3),
                Err(Corruption::InvalidCompressedData),
                "checksum byte {i} bit {bit}"
            );
        }
    }
    // The same through zstd's encoder: flip each checksum byte of a real compressed frame.
    let mut framed = zstd_with(&text, &[CParameter::ChecksumFlag(true)]);
    assert_eq!(
        decode(Codec::Zstd, &framed, text.len() as u64, 1 << 20, 3),
        Ok(text.clone())
    );
    let n = framed.len();
    for i in n - 4..n {
        framed[i] ^= 0x80;
        assert_eq!(
            decode(Codec::Zstd, &framed, text.len() as u64, 1 << 20, 3),
            Err(Corruption::InvalidCompressedData)
        );
        framed[i] ^= 0x80;
    }
}

#[test]
fn reserved_bits_and_block_types_are_rejected() {
    let text = text();
    let mut reserved_bit = Frame::raw(&text);
    reserved_bit.reserved_bit = true;
    let mut reserved_block = Frame::raw(b"x");
    reserved_block.blocks = vec![Block::Reserved];
    let mut raw_then_reserved = Frame::raw(&text);
    raw_then_reserved.blocks.push(Block::Reserved);
    raw_then_reserved.fcs = Fcs::Field(2, text.len() as u64 + 1);
    let len = text.len() as u64;
    for (f, content_len) in [
        (reserved_bit, len),
        (reserved_block, 1),
        (raw_then_reserved, len + 1),
    ] {
        assert_eq!(
            decode(Codec::Zstd, &f.encode(), content_len, 1 << 20, 3),
            Err(Corruption::InvalidCompressedData),
            "{f:?}"
        );
    }
}

proptest! {
    /// Random well-formed hand-crafted frames decode to their content; the same frame with a lying header
    /// length is rejected without touching `out`.
    #[test]
    fn hand_crafted_frames_round_trip(
        blocks in proptest::collection::vec(
            prop_oneof![
                proptest::collection::vec(any::<u8>(), 0..300).prop_map(Block::Raw),
                (any::<u8>(), 0_u32..3000).prop_map(|(b, n)| Block::Rle(b, n)),
            ],
            1..5,
        ),
        single_segment in any::<bool>(),
        wide_fcs in any::<bool>(),
        checksum in any::<bool>(),
        delta in prop_oneof![Just(0_i64), -2_i64..=2],
        offset in any::<u64>(),
    ) {
        let mut f = Frame::raw(b"");
        f.blocks = blocks;
        let content = f.content();
        let n = content.len() as u64;
        f.single_segment = single_segment;
        f.fcs = Fcs::Field(if wide_fcs { 8 } else { fcs_width(n).max(if single_segment { 1 } else { 2 }) }, n);
        if matches!(f.fcs, Fcs::Field(2, _)) && n < 256 {
            f.fcs = Fcs::Field(4, n);
        }
        f.checksum = checksum.then(|| checksum_of(&content));
        let claimed = n.saturating_add_signed(delta);
        let got = decode(Codec::Zstd, &f.encode(), claimed, 1 << 16, offset);
        if claimed == n {
            prop_assert_eq!(got, Ok(content));
        } else {
            prop_assert_eq!(got, Err(Corruption::UncompressedLenMismatch { expected: claimed, actual: n }));
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// 5. Thread-local context reuse
// ---------------------------------------------------------------------------------------------------------

/// What zstd produces at `level` from a context nobody used before.
fn one_shot(level: i32, src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(src.len()));
    zstd_safe::compress(&mut out, src, level).unwrap();
    out
}

#[derive(Debug, Clone)]
enum Op {
    Compress(i32, Vec<u8>),
    BadLevel,
    /// Decode a payload that fails partway through zstd's decoder (truncated mid-block or wrong checksum).
    FailedDecode(Vec<u8>, bool),
}

fn op() -> impl Strategy<Value = Op> {
    let data = prop_oneof![
        proptest::collection::vec((0_u8..4, 1_usize..60), 0..80).prop_map(|runs| runs
            .into_iter()
            .flat_map(|(b, n)| std::iter::repeat_n(b'a' + b, n))
            .collect()),
        proptest::collection::vec(any::<u8>(), 0..500),
    ];
    let level = prop_oneof![
        Just(-7),
        Just(-1),
        Just(1),
        Just(3),
        Just(9),
        Just(19),
        Just(22)
    ];
    prop_oneof![
        4 => (level, data.clone()).prop_map(|(l, d)| Op::Compress(l, d)),
        1 => Just(Op::BadLevel),
        2 => (data, any::<bool>()).prop_map(|(d, c)| Op::FailedDecode(d, c)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn cached_contexts_do_not_leak_state(ops in proptest::collection::vec(op(), 1..12)) {
        for op in ops {
            match op {
                Op::Compress(level, src) => {
                    let packed = zstd(level, &src);
                    prop_assert_eq!(&packed, &one_shot(level, &src), "level {}", level);
                    prop_assert_eq!(decode(Codec::Zstd, &packed, src.len() as u64, src.len(), 1), Ok(src));
                }
                Op::BadLevel => {
                    let mut out = vec![1];
                    prop_assert!(compress_block(Codec::Zstd, 1000, b"abc", &mut out).is_err());
                    prop_assert_eq!(out, [1]);
                }
                Op::FailedDecode(src, checksum) => {
                    let mut payload = zstd_with(&src, &[CParameter::ChecksumFlag(true)]);
                    let n = payload.len();
                    if checksum {
                        payload[n - 1] ^= 1;
                    } else {
                        // Keep the header intact so the decode reaches zstd's block decoder.
                        payload.truncate(n.max(10) - 5);
                    }
                    prop_assert!(decode(Codec::Zstd, &payload, src.len() as u64, 1 << 16, 2).is_err());
                }
            }
        }
    }
}

#[test]
fn decode_after_mid_stream_failure_is_correct() {
    let text = text();
    let good = zstd(19, &text);
    let mut bad = zstd(1, &b"other data that is decoded partially ".repeat(20));
    let mid = bad.len() / 2;
    bad[mid] ^= 0xff;
    let mut bad_sum = zstd_with(&text, &[CParameter::ChecksumFlag(true)]);
    let n = bad_sum.len();
    bad_sum[n - 2] ^= 4;
    for _ in 0..3 {
        let _ = decode(Codec::Zstd, &bad, 740, 1 << 16, 0);
        assert_eq!(
            decode(Codec::Zstd, &good, text.len() as u64, 1 << 16, 0),
            Ok(text.clone())
        );
        assert!(decode(Codec::Zstd, &bad_sum, text.len() as u64, 1 << 16, 0).is_err());
        assert_eq!(
            decode(Codec::Zstd, &good, text.len() as u64, 1 << 16, 0),
            Ok(text.clone())
        );
        // A frame that needs a dictionary must not make the context remember one.
        let mut dict = Frame::raw(&text);
        dict.dict = (4, 7);
        assert!(decode(Codec::Zstd, &dict.encode(), text.len() as u64, 1 << 16, 0).is_err());
        assert_eq!(
            decode(
                Codec::Zstd,
                &Frame::raw(&text).encode(),
                text.len() as u64,
                1 << 16,
                0
            ),
            Ok(text.clone())
        );
    }
}

// ---------------------------------------------------------------------------------------------------------
// 6. Every single-bit flip of a valid payload
// ---------------------------------------------------------------------------------------------------------

#[test]
fn every_single_bit_flip_is_safe() {
    let text = text();
    let mut multi = Frame::raw(&text[..50]);
    multi.blocks.push(Block::Rle(b'-', 300));
    multi.single_segment = false;
    multi.fcs = Fcs::Field(2, 350);
    let mut checked = Frame::raw(&text[..40]);
    checked.checksum = Some(checksum_of(&text[..40]));
    let payloads = [
        (text.clone(), zstd(DEFAULT_ZSTD_LEVEL, &text)),
        (text.clone(), zstd(19, &text)),
        (
            text.clone(),
            zstd_with(&text, &[CParameter::ChecksumFlag(true)]),
        ),
        (multi.content(), multi.encode()),
        (checked.content(), checked.encode()),
        (Vec::new(), zstd(DEFAULT_ZSTD_LEVEL, b"")),
    ];
    let mut rejected = 0;
    for (content, payload) in payloads {
        let len = content.len() as u64;
        for byte in 0..payload.len() {
            for bit in 0..8 {
                let mut flipped = payload.clone();
                flipped[byte] ^= 1 << bit;
                // `decode` asserts the contract: allowed error kind, caller's offset, `out` untouched, or
                // exactly `len` bytes appended.
                if decode(Codec::Zstd, &flipped, len, 1 << 16, 0xdead).is_err() {
                    rejected += 1;
                }
                // Every flip in the magic is rejected before allocation.
                if byte < 4 {
                    reject_early(&flipped, len, 1 << 16);
                }
            }
        }
    }
    assert!(rejected > 0);
}

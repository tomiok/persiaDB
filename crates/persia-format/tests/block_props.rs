//! Property and corruption tests for block-framed sections (ROADMAP 1.5.4; SPEC §5.2 "Block layout" and
//! "Block-framed sections"; ADR-0006). The golden bytes and the table rules are unit tests in `src/block.rs`.
//!
//! Invariants:
//! 1. Round trip: for any stream, `block_size` in `1..=MAX_BLOCK_SIZE`, write chunking and compression, the
//!    section opens with `len() == stream.len()`, `block_count == ceil(len / block_size)`, block `i` decodes to
//!    `stream[i·bs..min((i+1)·bs, len)]` (appended after what `out` held), any `read_range` returns exactly the
//!    matching slice, `block_range`s tile the section up to the table, and `position()` counts every byte
//!    written. Writing in pieces gives byte-identical output to writing at once, and encoding is deterministic.
//! 2. Block corruption is local: any single-bit flip inside block `i` (header or payload) leaves `open` working
//!    and makes `read_block(i)` (and any `read_range` touching it) fail with
//!    `CorruptBlock { index: i, reason: ChecksumMismatch }` at the block's start, with `out` untouched; every
//!    other block still decodes to its bytes.
//! 3. Table corruption is caught at `open`: any single-bit flip in the block table is `Error::Corrupt` (never
//!    `CorruptBlock`): a `ChecksumMismatch` at the table CRC, or for a flip in `block_count` an `UnexpectedEof`
//!    at the section start or a `ChecksumMismatch`.
//! 4. Arbitrary mutations never panic: `open` fails only with `Error::Corrupt`, block reads only with
//!    `CorruptBlock` naming the block, error offsets lie within `[base, base + section.len()]`, failed reads
//!    leave `out` untouched, and a successful read appends exactly the block's length per the table.
//! 5. Bomb guard: no decode appends more than `block_size` bytes per block, whatever the payload's frame
//!    claims; a table with `block_size > MAX_BLOCK_SIZE` (or a `total_len` no `u32` block count can cover) is
//!    rejected at `open`.

// Test-only helpers index, cast and unwrap on purpose (see reader_writer_props.rs).
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::panic
)]

mod common;

use common::{apply, mutation};
use persia_format::block::{
    BLOCK_HEADER_LEN, BlockHeader, BlockReader, BlockWriter, Compression, DEFAULT_BLOCK_SIZE,
    MAX_BLOCK_SIZE, TABLE_TRAILER_LEN,
};
use persia_format::compress::{Codec, DEFAULT_ZSTD_LEVEL};
use persia_format::{Corruption, Error, Writer};
use proptest::prelude::*;
use proptest::sample::Index;

// ---------------------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------------------

/// How a generated stream looks: noise (stored as `None` under zstd), text-like runs (stored as zstd), or
/// alternating stretches of both (so one section mixes codecs).
#[derive(Debug, Clone, Copy)]
enum Shape {
    Noise,
    Text,
    Mixed,
}

/// A deterministic stream of `len` bytes from `seed`: a structured generator keeps shrinking useful (it
/// shrinks `len`, not 64 KiB of bytes).
fn stream(shape: Shape, len: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&[u8]; 8] = [
        b"persia ",
        b"search ",
        b"segment ",
        b"query ",
        b"the ",
        b"of ",
        b"block ",
        b"zstd ",
    ];
    let mut x = seed | 1;
    let mut next = move || {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        x >> 33
    };
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        let r = next();
        let text = match shape {
            Shape::Noise => false,
            Shape::Text => true,
            Shape::Mixed => (out.len() / 300) % 2 == 0,
        };
        if text {
            out.extend_from_slice(WORDS[(r % 8) as usize]);
        } else {
            out.push(r as u8);
        }
    }
    out.truncate(len);
    out
}

fn shape() -> impl Strategy<Value = Shape> {
    prop_oneof![Just(Shape::Noise), Just(Shape::Text), Just(Shape::Mixed)]
}

fn compression() -> impl Strategy<Value = Compression> {
    prop_oneof![
        2 => Just(Compression::None),
        2 => Just(Compression::Zstd { level: DEFAULT_ZSTD_LEVEL }),
        1 => prop_oneof![Just(-5), Just(1), Just(9)].prop_map(|level| Compression::Zstd { level }),
    ]
}

/// Mostly small sizes (many blocks, short last blocks), sometimes the SPEC default.
fn block_size() -> impl Strategy<Value = u32> {
    prop_oneof![
        3 => 1_u32..=16,
        4 => 17_u32..=4096,
        1 => Just(DEFAULT_BLOCK_SIZE),
    ]
}

/// Stream lengths `0..=64 KiB`, capped at `max_blocks` blocks so tiny block sizes under zstd stay fast.
fn stream_len(block_size: u32, max_blocks: usize) -> impl Strategy<Value = usize> {
    let cap = (block_size as usize * max_blocks).min(64 * 1024);
    prop_oneof![
        1 => Just(0_usize),
        3 => 0..=cap.min(2048),
        2 => 0..=cap,
        // Exact multiples of the block size: the last block is full.
        1 => (0..=cap / block_size as usize).prop_map(move |n| n * block_size as usize),
    ]
}

/// A valid section's parameters: (`block_size`, compression, stream).
fn section_input(max_blocks: usize) -> impl Strategy<Value = (u32, Compression, Vec<u8>)> {
    block_size().prop_flat_map(move |bs| {
        (
            Just(bs),
            compression(),
            shape(),
            stream_len(bs, max_blocks),
            any::<u64>(),
        )
            .prop_map(|(bs, c, shape, len, seed)| (bs, c, stream(shape, len, seed)))
    })
}

/// Absolute section start, including the top of `u64`, where offsets saturate.
fn base() -> impl Strategy<Value = u64> {
    prop_oneof![
        Just(0_u64),
        0_u64..1 << 40,
        Just(u64::MAX - (1 << 20)),
        Just(u64::MAX)
    ]
}

fn build(block_size: u32, compression: Compression, data: &[u8]) -> Vec<u8> {
    let mut w = BlockWriter::new(block_size, compression).unwrap();
    w.write(data).unwrap();
    w.finish().unwrap()
}

/// The stream bytes block `i` holds (SPEC §5.2).
fn expected_block(data: &[u8], block_size: u32, i: u32) -> &[u8] {
    let bs = block_size as usize;
    let start = i as usize * bs;
    &data[start..(start + bs).min(data.len())]
}

fn to_usize(range: std::ops::Range<u64>) -> std::ops::Range<usize> {
    usize::try_from(range.start).unwrap()..usize::try_from(range.end).unwrap()
}

/// Start of the block table: `8·n + 24` bytes before the end (SPEC §5.2).
fn table_start(section: &[u8], block_count: u32) -> usize {
    section.len() - 8 * block_count as usize - TABLE_TRAILER_LEN
}

/// Whether `offset` lies in `[base, base + len]`; the reader saturates at `u64::MAX`.
fn within(offset: u64, base: u64, len: usize) -> bool {
    offset >= base && offset <= base.saturating_add(len as u64)
}

/// The block holding section byte `pos`, from the reader's own `block_range`s.
fn block_of(r: &BlockReader<'_>, pos: usize) -> u32 {
    (0..r.block_count())
        .find(|&i| to_usize(r.block_range(i).unwrap()).contains(&pos))
        .unwrap()
}

/// Expected error for an `open` of a section whose table has one flipped bit at `pos` (invariant 3).
fn assert_table_flip_rejected(section: &[u8], base: u64, pos: usize) -> Result<(), TestCaseError> {
    let len = section.len();
    let crc_offset = base.saturating_add(len as u64 - 4);
    let count_field = len - 12..len - 8;
    match BlockReader::open(section, base) {
        Err(Error::Corrupt {
            offset,
            reason: Corruption::ChecksumMismatch { .. },
        }) => prop_assert_eq!(offset, crc_offset, "flip at {}", pos),
        Err(Error::Corrupt {
            offset,
            reason: Corruption::UnexpectedEof { .. },
        }) if count_field.contains(&pos) => prop_assert_eq!(offset, base, "flip at {}", pos),
        other => prop_assert!(false, "table flip at {} gave {:?}", pos, other),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// 1. Round trip
// ---------------------------------------------------------------------------------------------------------

/// Checks every read of a valid section of `data` against the stream (invariant 1).
fn check_round_trip(
    section: &[u8],
    base: u64,
    block_size: u32,
    data: &[u8],
    ranges: &[(Index, Index)],
    prefix: &[u8],
) -> Result<(), TestCaseError> {
    let r = BlockReader::open(section, base).unwrap();
    let n = data.len().div_ceil(block_size as usize);
    prop_assert_eq!(r.len(), data.len() as u64);
    prop_assert_eq!(r.is_empty(), data.is_empty());
    prop_assert_eq!(r.block_size(), block_size);
    prop_assert_eq!(r.block_count() as usize, n);

    // The blocks tile the section from 0 up to the table; each is at least a header long.
    let mut prev_end = 0;
    let mut all = prefix.to_vec();
    for i in 0..r.block_count() {
        let range = r.block_range(i).unwrap();
        prop_assert_eq!(range.start, prev_end);
        prop_assert!(range.end - range.start >= BLOCK_HEADER_LEN as u64);
        prev_end = range.end;
        prop_assert_eq!(r.block_start(i), u64::from(i) * u64::from(block_size));

        let mut out = prefix.to_vec();
        r.read_block(i, &mut out).unwrap();
        prop_assert_eq!(&out[..prefix.len()], prefix);
        let want = expected_block(data, block_size, i);
        prop_assert!(want.len() <= block_size as usize && !want.is_empty());
        prop_assert_eq!(&out[prefix.len()..], want, "block {}", i);
        r.read_block(i, &mut all).unwrap();
    }
    prop_assert_eq!(prev_end as usize, table_start(section, r.block_count()));
    prop_assert_eq!(&all[prefix.len()..], data);
    prop_assert_eq!(
        r.read_block(r.block_count(), &mut Vec::new()),
        Err(Error::OutOfBounds {
            index: u64::from(r.block_count()),
            len: u64::from(r.block_count()),
        })
    );

    let total = data.len();
    let mut whole = prefix.to_vec();
    r.read_range(0, total as u64, &mut whole).unwrap();
    prop_assert_eq!(&whole[prefix.len()..], data);
    for (a, b) in ranges {
        let (a, b) = (a.index(total + 1), b.index(total + 1));
        let (start, end) = (a.min(b), a.max(b));
        let mut out = prefix.to_vec();
        r.read_range(start as u64, (end - start) as u64, &mut out)
            .unwrap();
        prop_assert_eq!(&out[..prefix.len()], prefix);
        prop_assert_eq!(
            &out[prefix.len()..],
            &data[start..end],
            "range {}..{}",
            start,
            end
        );
        // One byte past the stream end is out of bounds, and leaves `out` alone.
        let mut out = prefix.to_vec();
        prop_assert_eq!(
            r.read_range(start as u64, (total - start + 1) as u64, &mut out),
            Err(Error::OutOfBounds {
                index: total as u64 + 1,
                len: total as u64
            })
        );
        prop_assert_eq!(out, prefix);
    }
    Ok(())
}

/// Splits `data` at the sorted `cuts` (empty pieces included) and writes the pieces, checking `position()`.
fn write_in_pieces(
    block_size: u32,
    compression: Compression,
    data: &[u8],
    cuts: &[Index],
) -> Result<Vec<u8>, TestCaseError> {
    let mut at: Vec<usize> = cuts.iter().map(|c| c.index(data.len() + 1)).collect();
    at.sort_unstable();
    at.push(data.len());
    let mut w = BlockWriter::new(block_size, compression).unwrap();
    prop_assert_eq!(w.position(), 0);
    let mut from = 0;
    for to in at {
        w.write(&data[from..to]).unwrap();
        prop_assert_eq!(w.position(), to as u64);
        from = to;
    }
    Ok(w.finish().unwrap())
}

proptest! {
    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI")]
    fn round_trip(
        (bs, compression, data) in section_input(512),
        cuts in proptest::collection::vec(any::<Index>(), 0..12),
        ranges in proptest::collection::vec((any::<Index>(), any::<Index>()), 0..8),
        prefix in proptest::collection::vec(any::<u8>(), 0..4),
        base in base(),
    ) {
        let whole = build(bs, compression, &data);
        prop_assert_eq!(&build(bs, compression, &data), &whole, "encoding is deterministic");
        let pieces = write_in_pieces(bs, compression, &data, &cuts)?;
        prop_assert_eq!(&pieces, &whole, "write in pieces == write at once");
        check_round_trip(&whole, base, bs, &data, &ranges, &prefix)?;
    }

    /// Byte-at-a-time writes exercise the `pending` path on every call.
    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI")]
    fn byte_at_a_time_equals_write_at_once(
        bs in 1_u32..64,
        compression in compression(),
        shape in shape(),
        len in 0_usize..600,
        seed in any::<u64>(),
    ) {
        let data = stream(shape, len, seed);
        let mut w = BlockWriter::new(bs, compression).unwrap();
        for (i, b) in data.iter().enumerate() {
            w.write(std::slice::from_ref(b)).unwrap();
            prop_assert_eq!(w.position(), i as u64 + 1);
        }
        prop_assert_eq!(w.finish().unwrap(), build(bs, compression, &data));
    }
}

#[test]
#[cfg_attr(miri, ignore = "zstd is C behind FFI")]
fn largest_block_size_round_trips() {
    let data = stream(Shape::Mixed, MAX_BLOCK_SIZE as usize + 100, 7);
    for compression in [Compression::None, Compression::default()] {
        let section = build(MAX_BLOCK_SIZE, compression, &data);
        let r = BlockReader::open(&section, 0).unwrap();
        assert_eq!(r.block_count(), 2);
        let mut out = Vec::new();
        r.read_range(u64::from(MAX_BLOCK_SIZE) - 10, 20, &mut out)
            .unwrap();
        assert_eq!(
            out,
            data[MAX_BLOCK_SIZE as usize - 10..MAX_BLOCK_SIZE as usize + 10]
        );
    }
}

// ---------------------------------------------------------------------------------------------------------
// 2, 3. Single-bit corruption
// ---------------------------------------------------------------------------------------------------------

/// After flipping one bit at section byte `pos` (inside block `hit`): only block `hit` fails, with a checksum
/// mismatch at its start, and `out` untouched; ranges touching it fail the same way (invariant 2).
fn assert_block_flip_is_local(
    flipped: &[u8],
    base: u64,
    block_size: u32,
    data: &[u8],
    hit: u32,
) -> Result<(), TestCaseError> {
    let r = BlockReader::open(flipped, base).unwrap();
    let start = r.block_range(hit).unwrap().start;
    for i in 0..r.block_count() {
        let mut out = vec![0x5a];
        let got = r.read_block(i, &mut out);
        if i == hit {
            match got {
                Err(Error::CorruptBlock {
                    index,
                    offset,
                    reason: Corruption::ChecksumMismatch { .. },
                }) => {
                    prop_assert_eq!(index, hit);
                    prop_assert_eq!(offset, base.saturating_add(start));
                }
                other => prop_assert!(false, "flip in block {} gave {:?}", hit, other),
            }
            prop_assert_eq!(&out, &[0x5a]);
        } else {
            prop_assert_eq!(got, Ok(()));
            prop_assert_eq!(&out[1..], expected_block(data, block_size, i));
        }
    }
    // A range from just before to just after the broken block fails as a whole.
    let lo = r.block_start(hit).saturating_sub(1);
    let hi = (r.block_start(hit) + u64::from(block_size) + 1).min(r.len());
    let mut out = vec![0x5a];
    let got = r.read_range(lo, hi - lo, &mut out);
    prop_assert!(
        matches!(got, Err(Error::CorruptBlock { index, .. }) if index == hit),
        "range over block {}: {:?}",
        hit,
        got
    );
    prop_assert_eq!(&out, &[0x5a]);
    Ok(())
}

proptest! {
    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI")]
    fn bit_flip_in_a_block_is_caught_and_local(
        (bs, compression, data) in section_input(64).prop_filter("has blocks", |(_, _, d)| !d.is_empty()),
        block in any::<Index>(),
        byte in any::<Index>(),
        bit in 0_u8..8,
        base in base(),
    ) {
        let section = build(bs, compression, &data);
        let r = BlockReader::open(&section, base).unwrap();
        let hit = block.index(r.block_count() as usize) as u32;
        let range = to_usize(r.block_range(hit).unwrap());
        let pos = range.start + byte.index(range.len());
        let mut flipped = section.clone();
        flipped[pos] ^= 1 << bit;
        assert_block_flip_is_local(&flipped, base, bs, &data, hit)?;
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI")]
    fn bit_flip_in_the_table_fails_open(
        (bs, compression, data) in section_input(64),
        byte in any::<Index>(),
        bit in 0_u8..8,
        base in base(),
    ) {
        let section = build(bs, compression, &data);
        let n = data.len().div_ceil(bs as usize) as u32;
        let start = table_start(&section, n);
        let pos = start + byte.index(section.len() - start);
        let mut flipped = section;
        flipped[pos] ^= 1 << bit;
        assert_table_flip_rejected(&flipped, base, pos)?;
    }
}

/// Every bit of a few small sections, both codecs, including the empty section (invariants 2 and 3).
#[test]
#[cfg_attr(miri, ignore = "zstd is C behind FFI")]
fn every_single_bit_flip_is_detected() {
    let cases: [(u32, Compression, Vec<u8>); 5] = [
        (4, Compression::None, b"hello".to_vec()),
        (1, Compression::None, b"ab".to_vec()),
        (64, Compression::default(), stream(Shape::Mixed, 200, 3)),
        (300, Compression::default(), stream(Shape::Text, 300, 4)),
        (DEFAULT_BLOCK_SIZE, Compression::default(), Vec::new()),
    ];
    for (bs, compression, data) in cases {
        let section = build(bs, compression, &data);
        let r = BlockReader::open(&section, 77).unwrap();
        let start = table_start(&section, r.block_count());
        for pos in 0..section.len() {
            for bit in 0..8 {
                let mut flipped = section.clone();
                flipped[pos] ^= 1 << bit;
                if pos < start {
                    let hit = block_of(&r, pos);
                    assert_block_flip_is_local(&flipped, 77, bs, &data, hit)
                        .unwrap_or_else(|e| panic!("bs {bs}, pos {pos}, bit {bit}: {e}"));
                } else {
                    assert_table_flip_rejected(&flipped, 77, pos)
                        .unwrap_or_else(|e| panic!("bs {bs}, pos {pos}, bit {bit}: {e}"));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// 4. Arbitrary mutations
// ---------------------------------------------------------------------------------------------------------

/// Opens and reads a possibly corrupt section, checking the error contract (invariants 4 and 5). Returns the
/// decoded blocks (`None` for a failed one), or `None` if `open` failed.
fn read_hostile(section: &[u8], base: u64) -> Result<Option<Vec<Option<Vec<u8>>>>, TestCaseError> {
    let r = match BlockReader::open(section, base) {
        Ok(r) => r,
        Err(Error::Corrupt { offset, .. }) => {
            prop_assert!(
                within(offset, base, section.len()),
                "open error at {} (base {})",
                offset,
                base
            );
            return Ok(None);
        }
        Err(other) => {
            return Err(TestCaseError::fail(format!(
                "open fails only with Error::Corrupt: {other:?}"
            )));
        }
    };
    prop_assert!(r.block_size() >= 1 && r.block_size() <= MAX_BLOCK_SIZE);
    let mut blocks = Vec::new();
    for i in 0..r.block_count() {
        let range = r.block_range(i).unwrap();
        prop_assert!(range.end as usize <= section.len());
        let mut out = vec![1, 2, 3];
        match r.read_block(i, &mut out) {
            Ok(()) => {
                let want = r
                    .len()
                    .saturating_sub(r.block_start(i))
                    .min(u64::from(r.block_size()));
                prop_assert_eq!(out.len() as u64 - 3, want, "block {}", i);
                prop_assert!(want <= u64::from(r.block_size()), "bomb guard");
                blocks.push(Some(out.split_off(3)));
            }
            Err(Error::CorruptBlock { index, offset, .. }) => {
                prop_assert_eq!(index, i);
                prop_assert!(
                    within(offset, base, section.len()),
                    "block error at {} (base {})",
                    offset,
                    base
                );
                prop_assert_eq!(&out, &[1, 2, 3]);
                blocks.push(None);
            }
            Err(other) => prop_assert!(false, "block {} read gave {:?}", i, other),
        }
    }
    // A whole-stream read agrees with the per-block reads.
    let mut out = vec![9];
    let whole = r.read_range(0, r.len(), &mut out);
    if blocks.iter().all(Option::is_some) {
        prop_assert_eq!(whole, Ok(()));
        let concat: Vec<u8> = blocks.iter().flatten().flatten().copied().collect();
        prop_assert_eq!(&out[1..], &concat[..]);
    } else {
        let first_bad = blocks.iter().position(Option::is_none).unwrap() as u32;
        prop_assert!(
            matches!(whole, Err(Error::CorruptBlock { index, .. }) if index == first_bad),
            "{:?}",
            whole
        );
        prop_assert_eq!(&out, &[9]);
    }
    Ok(Some(blocks))
}

proptest! {
    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI")]
    fn mutated_sections_never_panic(
        (bs, compression, data) in section_input(32),
        m in mutation(),
        base in base(),
    ) {
        let section = build(bs, compression, &data);
        let mutated = apply(section.clone(), &m);
        let result = read_hostile(&mutated, base)?;
        if mutated == section {
            let blocks = result.expect("an unmutated section opens");
            let decoded: Vec<u8> = blocks.into_iter().map(Option::unwrap).collect::<Vec<_>>().concat();
            prop_assert_eq!(decoded, data);
        }
    }

    /// Pure noise with a valid-looking trailer: the CRC is fixed up so the table rules and block decoding are
    /// reached, not just the table checksum.
    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI")]
    fn noise_with_a_valid_table_crc_never_panics(
        blocks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 0..6),
        ends_jitter in proptest::collection::vec(-20_i64..20, 0..6),
        total_len in prop_oneof![0_u64..400, any::<u64>()],
        block_size in prop_oneof![1_u32..80, Just(MAX_BLOCK_SIZE), any::<u32>()],
        reserved in prop_oneof![4 => Just(0_u32), 1 => any::<u32>()],
        base in base(),
    ) {
        let mut section = Vec::new();
        let mut ends = Vec::new();
        for (i, b) in blocks.iter().enumerate() {
            section.extend(b);
            let jitter = ends_jitter.get(i).copied().unwrap_or(0);
            ends.push((section.len() as u64).saturating_add_signed(jitter));
        }
        section.extend(table(&ends, total_len, block_size, reserved));
        read_hostile(&section, base)?;
    }
}

// ---------------------------------------------------------------------------------------------------------
// 5. Bomb guard
// ---------------------------------------------------------------------------------------------------------

/// A hand-built block table (SPEC §5.2) with a valid CRC.
fn table(ends: &[u64], total_len: u64, block_size: u32, reserved: u32) -> Vec<u8> {
    let mut w = Writer::new();
    for &end in ends {
        w.write_u64(end);
    }
    w.write_u64(total_len);
    w.write_u32(block_size);
    w.write_u32(ends.len() as u32);
    w.write_u32(reserved);
    let crc = persia_format::checksum::crc32c(w.as_slice());
    w.write_u32(crc);
    w.into_inner()
}

/// One block with a correct CRC and the given header fields.
fn block(codec: Codec, uncompressed_len: u32, payload: &[u8]) -> Vec<u8> {
    let header = BlockHeader::with_uncompressed_len(codec, uncompressed_len, payload).unwrap();
    let mut w = Writer::new();
    header.encode(&mut w);
    w.write_bytes(payload);
    w.into_inner()
}

#[test]
fn table_block_size_above_the_maximum_is_rejected() {
    for size in [MAX_BLOCK_SIZE + 1, u32::MAX] {
        // One block of one byte: everything else about the table is valid.
        let mut section = block(Codec::None, 1, b"x");
        section.extend(table(&[17], 1, size, 0));
        let at = 1000 + section.len() as u64 - 24 + 8;
        assert_eq!(
            BlockReader::open(&section, 1000).unwrap_err(),
            Error::Corrupt {
                offset: at,
                reason: Corruption::InvalidBlockSize {
                    size,
                    max: MAX_BLOCK_SIZE
                }
            }
        );
    }
    // The maximum itself is valid.
    let mut section = block(Codec::None, 1, b"x");
    section.extend(table(&[17], 1, MAX_BLOCK_SIZE, 0));
    assert_eq!(BlockReader::open(&section, 0).unwrap().len(), 1);
}

#[test]
fn total_len_beyond_any_block_count_is_rejected() {
    let mut section = block(Codec::None, 1, b"x");
    section.extend(table(&[17], u64::MAX, MAX_BLOCK_SIZE, 0));
    assert!(matches!(
        BlockReader::open(&section, 0),
        Err(Error::Corrupt {
            reason: Corruption::BlockCountMismatch { count: 1, .. },
            ..
        })
    ));
}

/// A zstd frame that expands far beyond the block is rejected before `out` grows, whether its header claims
/// the table's length (the frame disagrees) or the frame's (the table disagrees).
#[test]
#[cfg_attr(miri, ignore = "zstd is C behind FFI")]
fn zstd_bomb_in_a_block_is_rejected_without_allocating() {
    const BS: u32 = 64;
    let mut bomb = Vec::new();
    persia_format::compress::compress_block(Codec::Zstd, 19, &vec![0; 1 << 20], &mut bomb).unwrap();
    // Without a declared content size too, so only the decoder's own limit can stop it.
    let no_size = zstd::bulk::Compressor::new(19)
        .and_then(|mut c| {
            c.set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(false))?;
            c.compress(&vec![0; 1 << 20])
        })
        .unwrap();
    for payload in [&bomb, &no_size] {
        for claimed in [BS, 1 << 20] {
            let mut section = block(Codec::Zstd, claimed, payload);
            let end = section.len() as u64;
            section.extend(table(&[end], u64::from(BS), BS, 0));
            let r = BlockReader::open(&section, 0).unwrap();
            let mut out = Vec::new();
            let err = r.read_block(0, &mut out).unwrap_err();
            assert!(
                matches!(
                    err,
                    Error::CorruptBlock {
                        index: 0,
                        reason: Corruption::UncompressedLenMismatch { .. }
                            | Corruption::InvalidCompressedData
                            | Corruption::BlockLenMismatch { .. },
                        ..
                    }
                ),
                "{err:?}"
            );
            assert_eq!(out, Vec::<u8>::new());
            assert!(
                out.capacity() <= BS as usize,
                "grew to {} bytes",
                out.capacity()
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Slow variants (nightly): more cases, longer streams, every mutation kind.
// ---------------------------------------------------------------------------------------------------------

#[cfg(feature = "slow")]
proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn slow_round_trip_many_blocks(
        (bs, compression, data) in section_input(16 * 1024),
        cuts in proptest::collection::vec(any::<Index>(), 0..64),
        ranges in proptest::collection::vec((any::<Index>(), any::<Index>()), 0..32),
        base in base(),
    ) {
        let whole = build(bs, compression, &data);
        prop_assert_eq!(&write_in_pieces(bs, compression, &data, &cuts)?, &whole);
        check_round_trip(&whole, base, bs, &data, &ranges, &[7])?;
    }

    #[test]
    fn slow_repeated_mutations_never_panic(
        (bs, compression, data) in section_input(128),
        ms in proptest::collection::vec(mutation(), 1..4),
        base in base(),
    ) {
        let mut bytes = build(bs, compression, &data);
        for m in &ms {
            if !matches!(m, common::Mutation::Keep) {
                bytes = apply(bytes, m);
            }
        }
        read_hostile(&bytes, base)?;
    }
}

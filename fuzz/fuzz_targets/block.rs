//! Fuzz block-framed sections (ROADMAP 1.5.4, SPEC §5.2 "Block layout" and "Block-framed sections", ADR-0006):
//! hostile sections never panic or over-allocate; `open` fails only with `Error::Corrupt`, block reads only with
//! `CorruptBlock` naming the block; error offsets lie within the section; failed reads leave `out` untouched;
//! a successful read appends exactly the block's length per the table (at most `block_size`, the bomb guard);
//! one flipped byte in a valid section is caught where it lands.
//!
//! Input: `mode u8, bs0 u8, bs1 u8, chunk u8, flip0 u8, flip1 u8, flip_xor u8, data..`. Even `mode`: `data` is the
//! section as is. Odd `mode`: `data` is the stream, written with `block_size = u16(bs) + 1` in pieces of
//! `chunk + 1` bytes, compressed with zstd (bit 1 of `mode`, level from the high bits) or not; then the section
//! byte at `u16(flip)` is XORed with `flip_xor`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::Error;
use persia_format::block::{BlockReader, BlockWriter, Compression, TABLE_TRAILER_LEN};

const BASE: u64 = 0x1234_5678;
const PREFIX: [u8; 3] = [0xaa, 0xbb, 0xcc];
/// Blocks decoded per input: a hostile zstd block may expand to `block_size` (up to 1 MiB), so cap the work.
const MAX_BLOCKS: u32 = 64;

fn within(offset: u64, len: usize) -> bool {
    offset >= BASE && offset <= BASE + len as u64
}

/// Reads every block (up to `MAX_BLOCKS`) and a few ranges, checking the error contract. Returns the decoded
/// blocks (`None` for a corrupt one), or `None` if `open` failed.
fn read_hostile(section: &[u8], range_seed: u64) -> Option<Vec<Option<Vec<u8>>>> {
    let r = match BlockReader::open(section, BASE) {
        Ok(r) => r,
        Err(Error::Corrupt { offset, .. }) => {
            assert!(within(offset, section.len()), "open error at {offset}");
            return None;
        }
        Err(other) => panic!("open fails only with Error::Corrupt: {other:?}"),
    };
    let mut prev_end = 0;
    for i in 0..r.block_count() {
        let range = r.block_range(i).expect("in-bounds block");
        assert_eq!(range.start, prev_end, "blocks tile the section");
        assert!(range.end as usize <= section.len());
        prev_end = range.end;
    }
    assert!(matches!(
        r.read_block(r.block_count(), &mut Vec::new()),
        Err(Error::OutOfBounds { .. })
    ));

    let mut blocks = Vec::new();
    let mut out = Vec::new();
    for i in 0..r.block_count().min(MAX_BLOCKS) {
        out.clear();
        out.extend_from_slice(&PREFIX);
        match r.read_block(i, &mut out) {
            Ok(()) => {
                let want = r
                    .len()
                    .saturating_sub(r.block_start(i))
                    .min(u64::from(r.block_size()));
                assert_eq!(
                    (out.len() - PREFIX.len()) as u64,
                    want,
                    "block {i} appends its table length"
                );
                assert_eq!(out[..PREFIX.len()], PREFIX);
                blocks.push(Some(out[PREFIX.len()..].to_vec()));
            }
            Err(Error::CorruptBlock { index, offset, .. }) => {
                assert_eq!(index, i);
                assert!(within(offset, section.len()), "block {i} error at {offset}");
                assert_eq!(out, PREFIX, "a failed read must not touch the output");
                blocks.push(None);
            }
            Err(other) => panic!("block {i} read gave {other:?}"),
        }
    }

    // A range inside the decoded blocks agrees with them.
    let covered = r.block_start(r.block_count().min(MAX_BLOCKS)).min(r.len());
    if covered > 0 {
        let start = range_seed % covered;
        let len = (range_seed >> 32) % (covered - start + 1);
        let mut out = PREFIX.to_vec();
        let result = r.read_range(start, len, &mut out);
        let bs = u64::from(r.block_size());
        let first = (start / bs) as usize;
        let last = if len == 0 {
            first
        } else {
            ((start + len - 1) / bs) as usize
        };
        if len == 0 || blocks[first..=last].iter().all(Option::is_some) {
            assert_eq!(result, Ok(()));
            let concat: Vec<u8> = blocks[first..=last]
                .iter()
                .flatten()
                .flatten()
                .copied()
                .collect();
            let lo = (start - first as u64 * bs) as usize;
            let expected = if len == 0 {
                &[][..]
            } else {
                &concat[lo..lo + len as usize]
            };
            assert_eq!(out[PREFIX.len()..], *expected);
        } else {
            assert!(
                matches!(result, Err(Error::CorruptBlock { .. })),
                "{result:?}"
            );
            assert_eq!(out, PREFIX);
        }
    }
    assert!(matches!(
        r.read_range(r.len(), 1, &mut Vec::new()),
        Err(Error::OutOfBounds { .. })
    ));
    Some(blocks)
}

fuzz_target!(|input: &[u8]| {
    let Some((header, data)) = input.split_first_chunk::<7>() else {
        return;
    };
    let [mode, bs0, bs1, chunk, f0, f1, flip_xor] = *header;
    let range_seed = u64::from_le_bytes([bs0, bs1, chunk, f0, f1, flip_xor, mode, 0x5a]);
    if mode & 1 == 0 {
        read_hostile(data, range_seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        return;
    }

    let block_size = u32::from(u16::from_le_bytes([bs0, bs1])) + 1;
    let compression = if mode & 2 == 2 {
        // Levels -3..=12: negative (fast) levels included, high ones skipped for speed.
        Compression::Zstd {
            level: i32::from(mode >> 2) % 16 - 3,
        }
    } else {
        Compression::None
    };
    let mut w = BlockWriter::new(block_size, compression).expect("valid parameters");
    for piece in data.chunks(usize::from(chunk) + 1) {
        w.write(piece).expect("in-memory write");
    }
    assert_eq!(w.position(), data.len() as u64);
    let mut section = w.finish().expect("in-memory finish");
    let block_count = data.len().div_ceil(block_size as usize);
    let table_start = section.len() - 8 * block_count - TABLE_TRAILER_LEN;

    let flip_at = usize::from(u16::from_le_bytes([f0, f1]));
    let flipped = flip_xor != 0 && flip_at < section.len();
    if flipped {
        section[flip_at] ^= flip_xor;
    }
    let blocks = read_hostile(&section, range_seed);

    if !flipped {
        let blocks = blocks.expect("an intact section opens");
        for (i, block) in blocks.iter().enumerate() {
            let start = i * block_size as usize;
            let end = (start + block_size as usize).min(data.len());
            assert_eq!(
                block.as_deref(),
                Some(&data[start..end]),
                "intact block {i}"
            );
        }
    } else if flip_at >= table_start {
        // A flipped table byte fails the table CRC (CRC32C catches every burst of up to 32 bits); a flipped
        // `block_count` moves the table start, so any table error may follow.
        let count_field = section.len() - 12..section.len() - 8;
        if !count_field.contains(&flip_at) {
            assert!(
                matches!(
                    BlockReader::open(&section, BASE),
                    Err(Error::Corrupt {
                        reason: persia_format::Corruption::ChecksumMismatch { .. },
                        ..
                    })
                ),
                "table flip at {flip_at}"
            );
        }
        assert!(blocks.is_none(), "table flip at {flip_at} opened");
    } else {
        // A flipped block byte fails that block's CRC, and only that block.
        let blocks = blocks.expect("the table is intact");
        let hit = flip_at;
        let r = BlockReader::open(&section, BASE).expect("the table is intact");
        for (i, block) in blocks.iter().enumerate() {
            let range = r.block_range(i as u32).expect("in-bounds block");
            if (range.start as usize..range.end as usize).contains(&hit) {
                assert!(block.is_none(), "flip at {hit} in block {i} not detected");
                assert!(matches!(
                    r.read_block(i as u32, &mut Vec::new()),
                    Err(Error::CorruptBlock {
                        reason: persia_format::Corruption::ChecksumMismatch { .. },
                        ..
                    })
                ));
            } else {
                let start = i * block_size as usize;
                let end = (start + block_size as usize).min(data.len());
                assert_eq!(block.as_deref(), Some(&data[start..end]), "block {i}");
            }
        }
    }
});

//! Fuzz block decompression (ROADMAP 1.4.3, SPEC §5.2 "Codecs", ADR-0005): random and mutated payloads never
//! panic or over-allocate; errors are at the caller's offset and leave `out` untouched; a success appends exactly
//! the declared length; an unmutated payload with its true length always round-trips.
//!
//! Input: `mode u8, codec u8, claimed u16, flip_at u16, flip_xor u8, data..`. Even `mode` (bit 0): `data` is the payload
//! as is (any codec byte, including unknown ones). Odd `mode`: `data` is first compressed with zstd at a level
//! from `mode`, then the byte at `flip_at` is XORed with `flip_xor`, so the mutations hit real frames.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::compress::{Codec, compress_block, decompress_block};
use persia_format::{Corruption, Error};
use zstd::zstd_safe;

/// Largest uncompressed length accepted: twice the default block size, so lying headers can exceed it.
const MAX_LEN: usize = 32 * 1024;
const OFFSET: u64 = 0x1234_5678;
const PREFIX: [u8; 3] = [0xaa, 0xbb, 0xcc];

fuzz_target!(|input: &[u8]| {
    let Some((header, data)) = input.split_first_chunk::<7>() else {
        return;
    };
    let [mode, codec_byte, c0, c1, f0, f1, flip_xor] = *header;
    let mut claimed = u64::from(u16::from_le_bytes([c0, c1]));
    let flip_at = usize::from(u16::from_le_bytes([f0, f1]));
    // Bit 1 of `mode`: claim the payload's own length (odd mode: the input's; even mode: the frame's declared
    // content size), so mutated frames get past the length pre-checks and into zstd's decoder.
    let honest = mode & 2 == 2;

    let (codec, payload, truth) = if mode & 1 == 1 {
        // Levels -3..=12: negative (fast) levels included, high ones skipped for speed.
        let level = i32::from(mode >> 2) % 16 - 3;
        let mut packed = Vec::new();
        compress_block(Codec::Zstd, level, data, &mut packed).expect("valid level compresses");
        if let Some(byte) = packed.get_mut(flip_at) {
            *byte ^= flip_xor;
        }
        if honest {
            claimed = data.len() as u64;
        }
        let intact = flip_xor == 0 || flip_at >= packed.len();
        (Codec::Zstd, packed, intact.then_some(data))
    } else {
        if honest {
            if let Ok(Some(declared)) = zstd_safe::get_frame_content_size(data) {
                claimed = declared;
            }
        }
        match Codec::from_u8(codec_byte, OFFSET) {
            Ok(codec) => (codec, data.to_vec(), None),
            Err(e) => {
                assert!(
                    matches!(e, Error::Corrupt { offset: OFFSET, reason: Corruption::UnknownCodec { codec } } if codec == codec_byte)
                );
                return;
            }
        }
    };

    let mut out = PREFIX.to_vec();
    match decompress_block(codec, &payload, claimed, MAX_LEN, OFFSET, &mut out) {
        Ok(()) => {
            assert_eq!(
                out.len() as u64,
                PREFIX.len() as u64 + claimed,
                "success must append exactly the declared length"
            );
            assert_eq!(out[..PREFIX.len()], PREFIX);
            if codec == Codec::None {
                assert_eq!(out[PREFIX.len()..], payload[..]);
            }
            if let Some(truth) = truth {
                assert_eq!(
                    &out[PREFIX.len()..],
                    truth,
                    "an intact frame decodes to its input"
                );
            }
        }
        Err(Error::Corrupt { offset, reason }) => {
            assert_eq!(offset, OFFSET);
            assert!(
                matches!(
                    reason,
                    Corruption::UncompressedTooLarge { .. }
                        | Corruption::UncompressedLenMismatch { .. }
                        | Corruption::InvalidCompressedData
                ),
                "{reason:?}"
            );
            assert_eq!(out, PREFIX, "a failed decode must not touch the output");
            // An intact frame is rejected only for a lying length or the bomb guard.
            if truth.is_some_and(|t| t.len() as u64 == claimed && t.len() <= MAX_LEN) {
                panic!("intact frame with its true length rejected: {reason:?}");
            }
        }
    }
});

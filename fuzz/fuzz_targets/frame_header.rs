//! Fuzz the ranged frame header and the streaming frame encoder (ROADMAP 1.7.5; SPEC §4.4 "Streaming and ranged
//! access"). `FrameHeader::decode` never panics, and at each probed position agrees with `Frame::decode` on the
//! same bytes: a valid frame has a header with its fields and length; with at least `MIN_FRAME_LEN` bytes a
//! failed header is the identical frame error; a header that runs past the input is the frame's `UnexpectedEof`
//! at `payload_len`; other frame failures sit behind the CRC (unknown flags are reported by the header, not
//! rejected); with fewer bytes both fail as `UnexpectedEof` at the frame; failures move neither reader.
//! `FrameEncoder` fed the data in chunks produces exactly `encode_frame(.., Compression::None)`, and rejects an
//! overrun without changing its output.
//!
//! Input: `mode u8, data..`. Mode bit 0: fix the CRC of every frame along the chain from the start; bit 1: fix the
//! CRC of every frame found at an 8-aligned offset (as in the `frame` target, whose seeds fit here). Bits 2..:
//! the chunk length (`1 + (mode >> 2)`) used to stream `data` through the encoder.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::block::Compression;
use persia_format::checksum::crc32c;
use persia_format::frame::{
    FLAG_ZSTD, FRAME_HEADER_LEN, FRAME_MAGIC, Frame, FrameEncoder, FrameHeader, FrameKind,
    KNOWN_FRAME_FLAGS, MIN_FRAME_LEN, encode_frame,
};
use persia_format::{Corruption, EncodeError, Error, Reader, Unsupported, Writer};

const BASE: u64 = 12288;
/// Positions probed besides the 8-aligned ones, so mid-field starts are covered without quadratic time.
const DENSE_PREFIX: usize = 64;

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// `24 + pad8(payload_len) + 8`, without overflow.
fn total_len(payload_len: u64) -> u128 {
    24 + u128::from(payload_len).div_ceil(8) * 8 + 8
}

/// Rewrites the CRC of the frame at `pos` if it has a magic and fits; returns its end.
fn fix_crc_at(b: &mut [u8], pos: usize) -> Option<usize> {
    let rest = b.get(pos..)?;
    if rest.len() < MIN_FRAME_LEN || rest[..4] != FRAME_MAGIC {
        return None;
    }
    let total = total_len(u64_at(rest, 8));
    if total > rest.len() as u128 {
        return None;
    }
    let end = pos + total as usize;
    let crc = crc32c(&b[pos + 4..end - 8]);
    b[end - 8..end - 4].copy_from_slice(&crc.to_le_bytes());
    Some(end)
}

fn check_position(data: &[u8], pos: usize) {
    let rest = &data[pos..];
    let o = BASE + pos as u64;
    let available = rest.len() as u64;
    let eof = |offset, needed| Error::Corrupt {
        offset,
        reason: Corruption::UnexpectedEof { needed, available },
    };

    let mut hr = Reader::with_base_offset(rest, o);
    let header = FrameHeader::decode(&mut hr);
    let mut fr = Reader::with_base_offset(rest, o);
    let frame = Frame::decode(&mut fr);

    match &header {
        Ok(h) => {
            assert_eq!(hr.position(), FRAME_HEADER_LEN, "a header reads 24 bytes");
            assert_eq!(rest[..4], FRAME_MAGIC);
            assert_eq!(h.kind().to_u8(), rest[4]);
            assert_eq!(
                h.flags(),
                u32::from_le_bytes([rest[5], rest[6], rest[7], 0])
            );
            assert_eq!(h.payload_len(), u64_at(rest, 8));
            assert_eq!(h.frame_seq(), u64_at(rest, 16));
            assert_eq!(u128::from(h.total_len()), total_len(h.payload_len()));
        }
        Err(_) => assert_eq!(hr.position(), 0, "a failed header moves nothing"),
    }
    match &frame {
        Ok(f) => assert_eq!(fr.position() as u64, f.total_len()),
        Err(_) => assert_eq!(fr.position(), 0, "a failed frame moves nothing"),
    }

    if rest.len() < FRAME_HEADER_LEN {
        assert_eq!(header.as_ref().err(), Some(&eof(o, 24)));
    }
    if rest.len() < MIN_FRAME_LEN {
        assert_eq!(frame.as_ref().err(), Some(&eof(o, 32)));
        return;
    }
    match (&header, &frame) {
        (Ok(h), Ok(f)) => {
            assert_eq!(h.kind(), f.kind());
            assert_eq!(h.frame_seq(), f.frame_seq());
            assert_eq!(h.flags() & FLAG_ZSTD != 0, f.is_compressed());
            assert_eq!(h.flags() & !KNOWN_FRAME_FLAGS, 0);
            assert_eq!(h.payload_len(), f.stored_payload().len() as u64);
            assert_eq!(h.total_len(), f.total_len());
        }
        (Err(he), Ok(_)) => panic!("a valid frame with a rejected header: {he:?}"),
        (Err(he), Err(fe)) => assert_eq!(he, fe, "header and frame disagree"),
        (Ok(h), Err(fe)) if h.total_len() > available => {
            assert_eq!(fe, &eof(o + 8, h.total_len()));
        }
        (Ok(h), Err(Error::Unsupported { offset, reason })) => {
            assert_eq!(*offset, o + 5);
            assert_eq!(
                reason,
                &Unsupported::FrameFlags {
                    flags: h.flags() & !KNOWN_FRAME_FLAGS
                }
            );
        }
        (Ok(_), Err(fe)) => assert!(
            matches!(
                fe.corruption(),
                Some((
                    _,
                    Corruption::ChecksumMismatch { .. } | Corruption::NonZeroReserved { .. }
                ))
            ),
            "a fitting frame fails only behind the CRC: {fe:?}"
        ),
    }
}

fn check_encoder(payload: &[u8], chunk: usize) {
    let declared = payload.len() as u64;
    let mut enc = FrameEncoder::new(FrameKind::Segment, declared, declared).unwrap();
    let mut out = enc.header().to_vec();
    for piece in payload.chunks(chunk) {
        // One byte too many is rejected and changes nothing.
        let over = vec![0xa5; (enc.remaining() + 1) as usize];
        let actual = declared + 1;
        assert_eq!(
            enc.update(&over),
            Err(EncodeError::PayloadLenMismatch { declared, actual })
        );
        enc.update(piece).unwrap();
        out.extend_from_slice(piece);
    }
    assert_eq!(enc.remaining(), 0);
    let total = enc.total_len();
    out.extend_from_slice(&enc.finish().unwrap());
    assert_eq!(out.len() as u64, total);
    let mut w = Writer::new();
    encode_frame(
        &mut w,
        FrameKind::Segment,
        declared,
        payload,
        Compression::None,
    )
    .unwrap();
    assert_eq!(
        out,
        w.as_slice(),
        "streamed frame differs from encode_frame"
    );
}

fuzz_target!(|input: &[u8]| {
    let Some((&mode, rest)) = input.split_first() else {
        return;
    };
    let mut data = rest.to_vec();
    if mode & 1 != 0 {
        let mut pos = 0;
        while let Some(end) = fix_crc_at(&mut data, pos) {
            pos = end;
        }
    }
    if mode & 2 != 0 {
        for pos in (0..data.len()).step_by(8) {
            fix_crc_at(&mut data, pos);
        }
    }
    for pos in (0..=data.len()).filter(|&p| p < DENSE_PREFIX || p % 8 == 0 || p + 32 > data.len()) {
        check_position(&data, pos);
    }
    check_encoder(rest, 1 + usize::from(mode >> 2));
});
